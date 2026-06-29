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
//! platform-specific decode backend; macOS mirrors it with Image I/O (`imageio.rs`).

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

use crate::isobmff::{color_from_colr_box, colr_transfer, is_hdr_transfer, isobmff_brand};
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

        // Preview-first: HEVC/AV1 full decode is slow (~250 ms for a 12 MP HEIC)
        // and the OS decoder barely parallelizes, so scrolling outruns it. The
        // embedded thumbnail decodes in ~ms — show it instantly, refine to the full
        // decode when the user lands. Falls through to the full decode if there's no
        // thumbnail (or it fails).
        if req.allow_preview {
            if let Ok((rgba, w, h, prim_w, prim_h)) = unsafe { wic_decode_thumbnail(req.bytes) } {
                // Like the primary frame, WIC's GetThumbnail returns the thumbnail
                // already display-oriented (container rotation applied), so pass
                // orientation 1 — re-applying it would double-rotate.
                if let Ok(mut img) = common::finalize_oriented(rgba, w, h, 1, codec, req.fit, true)
                {
                    // Report the *photo's* true resolution (the primary frame), not
                    // the thumbnail's, so the info panel / metadata is correct even
                    // before the full upgrade lands. `pixels` stays the thumbnail.
                    img.orig_width = prim_w;
                    img.orig_height = prim_h;
                    img.color = color_from_colr_box(req.bytes).unwrap_or_else(ColorTransform::srgb);
                    return Ok(img);
                }
            }
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

/// Decode the embedded thumbnail to RGBA8 via WIC (the fast preview path). Returns
/// `(rgba, thumb_w, thumb_h, primary_w, primary_h)` — the primary-frame size lets
/// the caller report the photo's *true* resolution while showing thumbnail pixels.
/// Errors if the file has no thumbnail (the caller falls back to the full decode).
/// `unsafe` because it drives COM.
unsafe fn wic_decode_thumbnail(
    bytes: &[u8],
) -> windows::core::Result<(Vec<u8>, u32, u32, u32, u32)> {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let factory: IWICImagingFactory =
        CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
    let stream = factory.CreateStream()?;
    stream.InitializeFromMemory(bytes)?;
    let decoder =
        factory.CreateDecoderFromStream(&stream, ptr::null(), WICDecodeMetadataCacheOnDemand)?;
    let frame = decoder.GetFrame(0)?;
    // The photo's true (display-oriented) resolution, from the primary frame.
    let (mut prim_w, mut prim_h) = (0u32, 0u32);
    frame.GetSize(&mut prim_w, &mut prim_h)?;
    let thumb = frame.GetThumbnail()?; // errs (WINCODEC_ERR_CODECNOTHUMBNAIL) if none

    let converter = factory.CreateFormatConverter()?;
    converter.Initialize(
        &thumb,
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
    Ok((buf, w, h, prim_w, prim_h))
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
