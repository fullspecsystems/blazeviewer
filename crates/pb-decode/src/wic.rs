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
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppRGBA, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};

use crate::{common, DecodeError, DecodeRequest, DecodedImage, ImageDecoder};

#[derive(Debug, Clone, Copy, Default)]
pub struct WicDecoder;

impl ImageDecoder for WicDecoder {
    fn can_decode(&self, bytes: &[u8]) -> bool {
        isobmff_brand(bytes).is_some()
    }

    fn decode(&self, req: &DecodeRequest) -> Result<DecodedImage, DecodeError> {
        let codec = isobmff_brand(req.bytes).unwrap_or("HEIF");
        let (rgba, w, h) =
            unsafe { wic_decode_rgba(req.bytes) }.map_err(|e| DecodeError::Corrupt(wic_msg(e)))?;
        // The WIC HEIF/AVIF decoder already applies the container's rotation (its
        // frame comes out display-oriented), so pass orientation 1 — re-applying
        // the EXIF Orientation here would double-rotate. Just decode-to-fit.
        common::finalize_oriented(rgba, w, h, 1, codec, req.fit, false)
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

/// Decode the first frame to RGBA8 via WIC. `unsafe` because it drives COM.
unsafe fn wic_decode_rgba(bytes: &[u8]) -> windows::core::Result<(Vec<u8>, u32, u32)> {
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
    Ok((buf, w, h))
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
}
