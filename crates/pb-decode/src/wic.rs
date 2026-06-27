//! Windows WIC backend for AVIF + HEIC/HEIF, via the OS imaging codecs.
//!
//! These formats need an AV1 / HEVC decoder, which has no mature pure-Rust
//! option and is painful to build from C on Windows (dav1d / libheif via vcpkg).
//! Windows already ships the decoders as installable codec packages (AV1 Video
//! Extension, HEVC Video Extensions, HEIF Image Extension), reachable through the
//! Windows Imaging Component — so this backend is just COM bindings (the pure-Rust
//! `windows` crate, no native build) and works whenever those extensions are
//! present. If a needed extension is missing, `CreateDecoderFromStream` fails and
//! the file is reported as a decode error, never a crash. This is the first
//! platform-specific decode backend; macOS would mirror it with ImageIO later.

use std::ptr;

use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat128bppRGBAFloat, GUID_WICPixelFormat32bppRGBA,
    IWICBitmapFrameDecode, IWICColorContext, IWICImagingFactory, WICBitmapDitherTypeNone,
    WICBitmapPaletteTypeCustom, WICColorContextExifColorSpace, WICColorContextProfile,
    WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};

use crate::{common, ColorTransform, DecodeError, DecodeRequest, DecodedImage, ImageDecoder};

#[derive(Debug, Clone, Copy, Default)]
pub struct WicDecoder;

impl ImageDecoder for WicDecoder {
    fn can_decode(&self, bytes: &[u8]) -> bool {
        isobmff_brand(bytes).is_some()
    }

    fn decode(&self, req: &DecodeRequest) -> Result<DecodedImage, DecodeError> {
        let codec = isobmff_brand(req.bytes).unwrap_or("HEIF");

        // HDR (PQ/HLG) sources: WIC's 8-bit converter truncates the high-range PQ
        // signal and it renders dark. Decode to float instead — WIC hands back
        // **scRGB-linear** (linear, BT.709 primaries, extended range) — and carry it
        // as fp16 so the renderer can present real HDR to an scRGB swapchain (or
        // tone-map for SDR displays).
        if colr_transfer(req.bytes).is_some_and(is_hdr_transfer) {
            let (floats, w, h) = unsafe { wic_decode_hdr(req.bytes) }
                .map_err(|e| DecodeError::Corrupt(wic_msg(e)))?;
            return common::finalize_hdr_scrgb(floats, w, h, codec, req.fit);
        }

        let (rgba, w, h, color) =
            unsafe { wic_decode_rgba(req.bytes) }.map_err(|e| DecodeError::Corrupt(wic_msg(e)))?;
        // The WIC HEIF/AVIF decoder already applies the container's rotation (its
        // frame comes out display-oriented), so pass orientation 1 — re-applying
        // the EXIF Orientation here would double-rotate. Just decode-to-fit.
        let mut img = common::finalize_oriented(rgba, w, h, 1, codec, req.fit, false)?;
        // WIC's format converter does NOT color-manage — it hands back the codec's
        // native pixels (Display-P3 for iPhone HEICs). The profile lives in the
        // container's color context, which we read for in-shader conversion.
        img.color = color;
        Ok(img)
    }

    fn name(&self) -> &'static str {
        "wic"
    }
}

/// Friendlier error text: a missing codec extension surfaces as a WIC
/// "component not found" HRESULT, which is the common real-world failure.
fn wic_msg(e: windows::core::Error) -> String {
    // WINCODEC_ERR_COMPONENTNOTFOUND
    if e.code().0 as u32 == 0x88982F50 {
        "WIC: no codec for this format (install the AV1 / HEVC / HEIF extension)".to_string()
    } else {
        format!("WIC: {e}")
    }
}

/// Decode the first frame to RGBA8 via WIC, plus its source color transform.
/// `unsafe` because it drives COM.
unsafe fn wic_decode_rgba(
    bytes: &[u8],
) -> windows::core::Result<(Vec<u8>, u32, u32, ColorTransform)> {
    // COM init per call: decode-pool workers start uninitialized. The HRESULT is
    // ignored on purpose — S_FALSE (already initialized) and RPC_E_CHANGED_MODE
    // (a differently-initialized apartment, e.g. the STA main thread) are both
    // usable for WIC.
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

    let factory: IWICImagingFactory =
        CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;

    let stream = factory.CreateStream()?;
    stream.InitializeFromMemory(bytes)?;

    let decoder =
        factory.CreateDecoderFromStream(&stream, ptr::null(), WICDecodeMetadataCacheOnDemand)?;
    let frame = decoder.GetFrame(0)?;

    // Source color profile. The Microsoft HEIF decoder does NOT expose color
    // contexts (GetColorContexts returns 0), so the ISOBMFF `colr` box is the
    // primary source; the WIC context query is a fallback (e.g. for AVIF).
    let color = match color_from_colr_box(bytes) {
        Some(c) => c,
        None => wic_color_transform(&factory, &frame),
    };

    // Convert whatever the codec yields (often BGRA) to straight RGBA8.
    let converter = factory.CreateFormatConverter()?;
    converter.Initialize(
        &frame,
        &GUID_WICPixelFormat32bppRGBA,
        WICBitmapDitherTypeNone,
        None,
        0.0,
        WICBitmapPaletteTypeCustom,
    )?;

    let (mut w, mut h) = (0u32, 0u32);
    converter.GetSize(&mut w, &mut h)?;
    let stride = w.saturating_mul(4);
    let mut buf = vec![0u8; stride as usize * h as usize];
    converter.CopyPixels(ptr::null(), stride, &mut buf)?;
    Ok((buf, w, h, color))
}

/// Decode an HDR (PQ/HLG) frame to **scRGB-linear f32** (linear light, BT.709
/// primaries, extended range) via WIC's float path. WIC applies the PQ/HLG decode +
/// gamut conversion; we keep the float values for the HDR output path.
/// `unsafe` because it drives COM.
unsafe fn wic_decode_hdr(bytes: &[u8]) -> windows::core::Result<(Vec<f32>, u32, u32)> {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let factory: IWICImagingFactory =
        CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
    let stream = factory.CreateStream()?;
    stream.InitializeFromMemory(bytes)?;
    let decoder =
        factory.CreateDecoderFromStream(&stream, ptr::null(), WICDecodeMetadataCacheOnDemand)?;
    let frame = decoder.GetFrame(0)?;

    let converter = factory.CreateFormatConverter()?;
    converter.Initialize(
        &frame,
        &GUID_WICPixelFormat128bppRGBAFloat,
        WICBitmapDitherTypeNone,
        None,
        0.0,
        WICBitmapPaletteTypeCustom,
    )?;
    let (mut w, mut h) = (0u32, 0u32);
    converter.GetSize(&mut w, &mut h)?;
    let stride = w.saturating_mul(16); // 4 channels * f32
    let mut floats = vec![0f32; (w as usize) * (h as usize) * 4];
    let byte_buf = std::slice::from_raw_parts_mut(floats.as_mut_ptr() as *mut u8, floats.len() * 4);
    converter.CopyPixels(ptr::null(), stride, byte_buf)?;
    Ok((floats, w, h))
}

/// Whether a CICP transfer characteristic is HDR (needs the float decode path):
/// 16 = SMPTE-2084 (PQ), 18 = ARIB STD-B67 (HLG).
fn is_hdr_transfer(transfer: u8) -> bool {
    matches!(transfer, 16 | 18)
}

/// The transfer characteristic from an `nclx` `colr` box, if present. (Embedded-ICC
/// `prof` HDR is rare and not detected here.) Same byte-scan as the color parse.
fn colr_transfer(bytes: &[u8]) -> Option<u8> {
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

/// Read the frame's WIC color context into a [`ColorTransform`]. This is the
/// fallback after the `colr`-box parse: the Microsoft HEIF decoder returns no
/// color contexts, but AVIF/other frames may. Any failure or absence falls back to
/// sRGB passthrough — wrong-but-shown beats not-shown.
unsafe fn wic_color_transform(
    factory: &IWICImagingFactory,
    frame: &IWICBitmapFrameDecode,
) -> ColorTransform {
    let mut count = 0u32;
    // First call (empty array) reports how many color contexts the frame has.
    if frame.GetColorContexts(&mut [], &mut count).is_err() || count == 0 {
        return ColorTransform::srgb();
    }
    let mut contexts: Vec<Option<IWICColorContext>> = Vec::with_capacity(count as usize);
    for _ in 0..count {
        match factory.CreateColorContext() {
            Ok(c) => contexts.push(Some(c)),
            Err(_) => return ColorTransform::srgb(),
        }
    }
    if frame.GetColorContexts(&mut contexts, &mut count).is_err() {
        return ColorTransform::srgb();
    }
    for ctx in contexts.iter().flatten() {
        let Ok(kind) = ctx.GetType() else { continue };
        if kind == WICColorContextProfile {
            // Size query (empty buffer) then fetch the ICC bytes.
            let mut size = 0u32;
            let _ = ctx.GetProfileBytes(&mut [], &mut size);
            if size == 0 {
                continue;
            }
            let mut icc = vec![0u8; size as usize];
            if ctx.GetProfileBytes(&mut icc, &mut size).is_ok() {
                return ColorTransform::from_icc(&icc);
            }
        } else if kind == WICColorContextExifColorSpace {
            if let Ok(code) = ctx.GetExifColorSpace() {
                let t = ColorTransform::from_exif_color_space(code);
                if t.enabled {
                    return t;
                }
            }
        }
    }
    ColorTransform::srgb()
}

/// Parse the source color transform from the ISOBMFF `colr` box (HEIF/AVIF). The
/// WIC HEIF decoder doesn't surface color contexts, so this is the primary path
/// for HEIC. Handles `prof`/`rICC` (embedded ICC — the common iPhone Display-P3
/// case) and `nclx` (CICP code points). Returns an *enabled* transform only, so an
/// sRGB/unparseable box leaves the caller on its fallback.
///
/// The box lives in `meta`→`iprp`→`ipco` near the file head; rather than walk the
/// full box tree we scan for the `colr` signature and validate the candidate (an
/// embedded ICC's own size header must match the box) — the same pragmatic
/// byte-scan style as `isobmff_brand` and the RAW JPEG-span finder.
fn color_from_colr_box(bytes: &[u8]) -> Option<ColorTransform> {
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

/// If `bytes` is an ISOBMFF (`ftyp`) image this backend handles — AVIF or
/// HEIC/HEIF — return its display codec label; else `None`. JXL's container opens
/// with a different box, so it never matches here.
fn isobmff_brand(bytes: &[u8]) -> Option<&'static str> {
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
