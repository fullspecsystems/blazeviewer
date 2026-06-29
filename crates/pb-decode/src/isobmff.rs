//! Pure ISOBMFF (HEIF/AVIF container) parsing shared by every HEVC/AV1 backend.
//!
//! These are plain byte-scanners — no platform, no decode — so the WIC backend
//! (Windows), the Image I/O backend (macOS), and the libheif backend all share one
//! tested implementation: brand sniffing (`isobmff_brand`), the source color
//! transform from the `colr` box (`color_from_colr_box`), and the HDR transfer
//! detection (`colr_transfer` + `is_hdr_transfer`).
//!
//! Rather than walk the full box tree we scan for the signature and validate the
//! candidate (an embedded ICC's own size header must match the box) — the same
//! pragmatic byte-scan style as the RAW JPEG-span finder.

// Only the `colr`-box color parser (Windows/libheif + tests) needs this; the brand
// and HDR-transfer sniffers are pure byte scans.
#[cfg(any(windows, test))]
use crate::ColorTransform;

/// If `bytes` is an ISOBMFF (`ftyp`) image these backends handle — AVIF or
/// HEIC/HEIF — return its display codec label; else `None`. JXL's container opens
/// with a different box, so it never matches here.
pub(crate) fn isobmff_brand(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() < 16 || &bytes[4..8] != b"ftyp" {
        return None;
    }
    fn classify(b: &[u8]) -> Option<&'static str> {
        match b {
            b"avif" | b"avis" => Some("AVIF"),
            b"heic" | b"heix" | b"heim" | b"heis" | b"hevc" | b"hevx" => Some("HEIC"),
            _ => None,
        }
    }
    // Major brand first, then the compatible-brands list (offset 16, after the
    // 4-byte minor version), bounded by the ftyp box length.
    if let Some(c) = classify(&bytes[8..12]) {
        return Some(c);
    }
    let box_len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let end = box_len.min(bytes.len());
    let mut heif = matches!(&bytes[8..12], b"mif1" | b"msf1");
    let mut i = 16;
    while i + 4 <= end {
        if let Some(c) = classify(&bytes[i..i + 4]) {
            return Some(c);
        }
        if matches!(&bytes[i..i + 4], b"mif1" | b"msf1" | b"miaf") {
            heif = true;
        }
        i += 4;
    }
    heif.then_some("HEIF")
}

/// Whether a CICP transfer characteristic is HDR (needs the float decode path):
/// 16 = SMPTE-2084 (PQ), 18 = ARIB STD-B67 (HLG).
pub(crate) fn is_hdr_transfer(transfer: u8) -> bool {
    matches!(transfer, 16 | 18)
}

/// The transfer characteristic from an `nclx` `colr` box, if present. (Embedded-ICC
/// `prof` HDR is rare and not detected here.) Same byte-scan as the color parse.
pub(crate) fn colr_transfer(bytes: &[u8]) -> Option<u8> {
    let mut i = 4usize;
    while i + 12 <= bytes.len() {
        if &bytes[i..i + 4] == b"colr" && &bytes[i + 4..i + 8] == b"nclx" {
            // transfer is the second u16 of the nclx payload (after primaries).
            return Some(*bytes.get(i + 11)?); // low byte of the transfer u16
        }
        i += 1;
    }
    None
}

/// Parse the source color transform from the ISOBMFF `colr` box (HEIF/AVIF). The
/// container decoders (WIC, Image I/O drawn into a matching space, libheif) hand
/// back source-native pixels, so this transform tells the shader how to convert
/// them to BT.709. Handles `prof`/`rICC` (embedded ICC — the common iPhone
/// Display-P3 case) and `nclx` (CICP code points). Returns an *enabled* transform
/// only, so an sRGB/unparseable box leaves the caller on its fallback.
///
/// The box lives in `meta`→`iprp`→`ipco` near the file head; we scan for the `colr`
/// signature and validate the candidate (an embedded ICC's own size header must
/// match the box).
///
/// Only the WIC/libheif backends (Windows) consult this — the macOS Image I/O backend
/// lets CoreGraphics color-manage into a fixed Display-P3 space instead — so it's
/// gated to its real consumers (plus `test`, so the parser tests run everywhere).
#[cfg(any(windows, test))]
pub(crate) fn color_from_colr_box(bytes: &[u8]) -> Option<ColorTransform> {
    let mut i = 4usize; // a valid box has its 4-byte size *before* the type
    while i + 8 <= bytes.len() {
        if &bytes[i..i + 4] == b"colr" {
            if let Some(t) = parse_colr_at(bytes, i) {
                return Some(t);
            }
        }
        i += 1;
    }
    None
}

/// Parse a `colr` box whose type field starts at `pos` (the 4-byte size precedes
/// it). `None` if it isn't a usable color box (lets the scan keep looking).
#[cfg(any(windows, test))]
fn parse_colr_at(bytes: &[u8], pos: usize) -> Option<ColorTransform> {
    let box_start = pos.checked_sub(4)?;
    let size = u32::from_be_bytes(bytes[box_start..pos].try_into().ok()?) as usize;
    if size < 12 || box_start + size > bytes.len() {
        return None;
    }
    let end = box_start + size;
    match &bytes[pos + 4..pos + 8] {
        b"prof" | b"rICC" => {
            let icc = &bytes[pos + 8..end];
            // Validate against the ICC's own size header to reject false matches
            // (a stray "colr" in unrelated data won't have a self-consistent ICC).
            let icc_size = u32::from_be_bytes(icc.get(0..4)?.try_into().ok()?) as usize;
            if icc_size != icc.len() {
                return None;
            }
            let t = ColorTransform::from_icc(icc);
            t.enabled.then_some(t)
        }
        b"nclx" => {
            // primaries:u16, transfer:u16, matrix:u16, then full-range flag (bit 7).
            let prim = u16::from_be_bytes(bytes.get(pos + 8..pos + 10)?.try_into().ok()?);
            let trc = u16::from_be_bytes(bytes.get(pos + 10..pos + 12)?.try_into().ok()?);
            let matx = u16::from_be_bytes(bytes.get(pos + 12..pos + 14)?.try_into().ok()?);
            let full = (*bytes.get(pos + 14)? & 0x80) != 0;
            let t = ColorTransform::from_cicp(prim as u8, trc as u8, matx as u8, full);
            t.enabled.then_some(t)
        }
        _ => None,
    }
}

/// Whether the HEIF embeds a **real** thumbnail item (a `thmb` reference in the
/// `iref` box). When it does, a fast thumbnail decode is available. When it does
/// NOT (e.g. macOS-encoded Sony HEICs), a decoder may *synthesize* one by decoding
/// the whole HEVC grid — as slow as a full decode — so the libheif backend should
/// handle those itself rather than pay that twice.
///
/// Pragmatic byte-scan, bounded to the file head where `meta`/`iref` always live.
/// Only the libheif backend consults this, so it's gated to that feature to stay
/// dead-code-free in builds without it.
#[cfg(feature = "libheif")]
pub(crate) fn has_thumbnail_ref(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(64 * 1024)];
    head.windows(4).any(|w| w == b"thmb")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ftyp(major: &[u8; 4], compat: &[&[u8; 4]]) -> Vec<u8> {
        let len = 8 + 4 + 4 + 4 * compat.len();
        let mut v = (len as u32).to_be_bytes().to_vec();
        v.extend_from_slice(b"ftyp");
        v.extend_from_slice(major);
        v.extend_from_slice(&[0, 0, 0, 0]); // minor version
        for c in compat {
            v.extend_from_slice(*c);
        }
        v
    }

    #[test]
    fn classifies_brands() {
        assert_eq!(isobmff_brand(&ftyp(b"avif", &[])), Some("AVIF"));
        assert_eq!(isobmff_brand(&ftyp(b"heic", &[])), Some("HEIC"));
        // major mif1 + compatible heic → HEIC wins over generic HEIF.
        assert_eq!(isobmff_brand(&ftyp(b"mif1", &[b"heic"])), Some("HEIC"));
        assert_eq!(isobmff_brand(&ftyp(b"mif1", &[b"miaf"])), Some("HEIF"));
        assert_eq!(isobmff_brand(b"\x89PNG\r\n\x1a\n____"), None);
        assert_eq!(isobmff_brand(&[0u8; 4]), None);
    }

    /// Wrap `colour_type` + `payload` in a `colr` box, prefixed by 8 bytes of
    /// padding so the box doesn't start at offset 0 (mirrors a real file, where
    /// the box's 4-byte size precedes the `colr` type).
    fn colr_box(colour_type: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = 4 + 4 + 4 + payload.len();
        let mut v = vec![0u8; 8]; // stand-in for the enclosing boxes
        v.extend_from_slice(&(size as u32).to_be_bytes());
        v.extend_from_slice(b"colr");
        v.extend_from_slice(colour_type);
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn colr_nclx_display_p3_is_detected() {
        // primaries=12 (SMPTE-432 / Display-P3), transfer=13 (sRGB), matrix=0.
        let payload = [0, 12, 0, 13, 0, 0, 0x80];
        let buf = colr_box(b"nclx", &payload);
        let t = color_from_colr_box(&buf).expect("nclx P3 should parse");
        assert!(t.enabled);
        assert!(
            t.matrix[1][0] < 0.0 && t.matrix[2][0] < 0.0,
            "P3 matrix {:?}",
            t.matrix
        );
    }

    #[test]
    fn colr_nclx_srgb_yields_no_transform() {
        // primaries=1 (BT.709), transfer=13 (sRGB) → plain sRGB → no conversion.
        let buf = colr_box(b"nclx", &[0, 1, 0, 13, 0, 0, 0x80]);
        assert!(color_from_colr_box(&buf).is_none());
    }

    #[test]
    fn colr_prof_embedded_p3_icc_is_detected() {
        // A real Display-P3 ICC (emitted by moxcms) wrapped in a `colr`/`prof` box.
        let icc = moxcms::ColorProfile::new_display_p3()
            .encode()
            .expect("encode P3 ICC");
        let buf = colr_box(b"prof", &icc);
        let t = color_from_colr_box(&buf).expect("prof P3 should parse");
        assert!(t.enabled);
        assert!(t.matrix[1][0] < 0.0, "P3 matrix {:?}", t.matrix);
    }

    #[test]
    fn colr_prof_with_inconsistent_icc_size_is_rejected() {
        // ICC whose own size header doesn't match the box payload → false match.
        let mut bogus = vec![0u8; 40];
        bogus[0..4].copy_from_slice(&9999u32.to_be_bytes()); // lies about its size
        let buf = colr_box(b"prof", &bogus);
        assert!(color_from_colr_box(&buf).is_none());
    }

    #[test]
    fn stray_colr_bytes_are_ignored() {
        // The literal "colr" with no valid box around it must not be mistaken.
        let buf = b"....colr????....".to_vec();
        assert!(color_from_colr_box(&buf).is_none());
    }

    #[test]
    fn detects_hdr_transfer_from_nclx() {
        assert!(is_hdr_transfer(16)); // PQ
        assert!(is_hdr_transfer(18)); // HLG
        assert!(!is_hdr_transfer(13)); // sRGB
        assert!(!is_hdr_transfer(1)); // BT.709

        // PQ Display-P3 nclx: primaries=12, transfer=16.
        let pq = colr_box(b"nclx", &[0, 12, 0, 16, 0, 0, 0x80]);
        assert_eq!(colr_transfer(&pq), Some(16));
        assert!(colr_transfer(&pq).is_some_and(is_hdr_transfer));

        // SDR Display-P3 nclx: transfer=13 → not HDR.
        let sdr = colr_box(b"nclx", &[0, 12, 0, 13, 0, 0, 0x80]);
        assert_eq!(colr_transfer(&sdr), Some(13));
        assert!(!colr_transfer(&sdr).is_some_and(is_hdr_transfer));
    }
}
