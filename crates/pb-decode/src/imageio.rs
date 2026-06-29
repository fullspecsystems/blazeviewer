//! macOS Image I/O backend for HEIC + AVIF (+ HEIF), via `CGImageSource`.
//!
//! The Mac mirror of `wic.rs`: HEVC/AV1 have no mature pure-Rust decoder, but macOS
//! ships hardware-assisted ones in Image I/O. Two advantages over bundling libheif
//! on this platform: it's the OS decoder (fast, hardware-assisted on Apple Silicon),
//! and it's **patent-clean** (Apple holds the HEVC license), so the Mac path sidesteps
//! the HEVC-patent exposure bundled libheif carries. Selected by the dispatcher on
//! macOS the way WIC is on Windows; if a needed codec is somehow unavailable,
//! `CGImageSourceCreateImageAtIndex` returns null and we report a decode error.
//!
//! Hand-rolled FFI (no new crate deps), the same style as `libheif.rs`. The handful
//! of CoreFoundation / Image I/O / CoreGraphics functions used here are stable C ABI.
//!
//! Color handling differs from WIC in one way: WIC hands back source-native pixels,
//! but CoreGraphics color-manages on draw. So we draw into a **Display-P3** context
//! (8-bit SDR) — preserving the P3 gamut for any source — and carry a fixed
//! **P3 → BT.709** transform for the in-shader CMS. HDR (PQ/HLG) draws into an
//! **extended-linear-sRGB** float context (scene-linear, extended range), matching
//! the renderer's scRGB intermediate.

use std::ffi::c_void;

use crate::isobmff::{colr_transfer, is_hdr_transfer, isobmff_brand};
use crate::{common, ColorTransform, DecodeError, DecodeRequest, DecodedImage, ImageDecoder};

#[derive(Debug, Clone, Copy, Default)]
pub struct ImageIoDecoder;

impl ImageDecoder for ImageIoDecoder {
    fn can_decode(&self, bytes: &[u8]) -> bool {
        isobmff_brand(bytes).is_some()
    }

    fn decode(&self, req: &DecodeRequest) -> Result<DecodedImage, DecodeError> {
        let codec = isobmff_brand(req.bytes).unwrap_or("HEIF");

        // HDR (PQ/HLG): decode to scene-linear extended-sRGB float and carry it as
        // fp16, so the renderer can present real HDR to a wide-gamut/EDR surface (or
        // tone-map for SDR displays). Orientation (see below) is applied to the float
        // buffer here, since `finalize_hdr_scrgb` assumes upright. Brightness
        // calibration vs the panel's EDR is validated on-device (task #4).
        if colr_transfer(req.bytes).is_some_and(is_hdr_transfer) {
            let (floats, w, h, orient) = unsafe { imageio_decode_hdr(req.bytes) }
                .ok_or_else(|| DecodeError::Corrupt("Image I/O: HDR decode failed".into()))?;
            let (floats, w, h) = orient_f32(floats, w, h, orient);
            return common::finalize_hdr_scrgb(floats, w, h, codec, req.fit);
        }

        // SDR: draw into a Display-P3 8-bit context (preserves wide gamut for any
        // source), then carry the fixed P3 → BT.709 transform.
        let (rgba, w, h, orient) =
            unsafe { imageio_decode_rgba8_p3(req.bytes) }.ok_or_else(|| {
                DecodeError::Corrupt(
                    "Image I/O: decode failed (unsupported or corrupt HEIC/AVIF)".into(),
                )
            })?;
        // `CGImageSourceCreateImageAtIndex` does NOT bake orientation — and unlike a
        // plain JPEG, HEIC/AVIF often carry rotation in the ISOBMFF `irot` transform,
        // not EXIF (so kamadak would read 1 and the photo shows sideways). We read
        // ImageIO's own `kCGImagePropertyOrientation`, which combines `irot` + EXIF,
        // and apply it here (pass it through, not the container bytes).
        let mut img = common::finalize_oriented(rgba, w, h, orient, codec, req.fit, false)?;
        // We forced a Display-P3 draw, so pixels are P3-encoded regardless of the
        // source profile — carry the matching P3(SMPTE-432) → BT.709 + sRGB-TRC transform.
        img.color = ColorTransform::from_cicp(12, 13, 0, true);
        Ok(img)
    }

    fn name(&self) -> &'static str {
        "imageio"
    }
}

// --- Minimal CoreFoundation / Image I/O / CoreGraphics FFI -------------------------

type CFTypeRef = *const c_void;
type CFDataRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFStringRef = *const c_void;
type CGImageSourceRef = *const c_void;
type CGImageRef = *const c_void;
type CGColorSpaceRef = *const c_void;
type CGContextRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

// CGBitmapInfo / CGImageAlphaInfo bits we use.
const ALPHA_PREMULTIPLIED_LAST: u32 = 1; // kCGImageAlphaPremultipliedLast
const FLOAT_COMPONENTS: u32 = 1 << 8; // kCGBitmapFloatComponents
const BYTE_ORDER_32_LITTLE: u32 = 2 << 12; // kCGBitmapByteOrder32Little (host on aarch64)

/// `kCFNumberIntType` — read a CFNumber into a C `int` (i32).
const CF_NUMBER_INT_TYPE: isize = 9;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFDataCreate(allocator: CFTypeRef, bytes: *const u8, length: isize) -> CFDataRef;
    fn CFRelease(cf: CFTypeRef);
    fn CFDictionaryGetValue(dict: CFDictionaryRef, key: *const c_void) -> *const c_void;
    fn CFNumberGetValue(number: *const c_void, the_type: isize, value: *mut c_void) -> bool;
}

#[link(name = "ImageIO", kind = "framework")]
extern "C" {
    static kCGImagePropertyOrientation: CFStringRef;
    fn CGImageSourceCreateWithData(data: CFDataRef, options: CFDictionaryRef) -> CGImageSourceRef;
    fn CGImageSourceCreateImageAtIndex(
        isrc: CGImageSourceRef,
        index: usize,
        options: CFDictionaryRef,
    ) -> CGImageRef;
    fn CGImageSourceCopyPropertiesAtIndex(
        isrc: CGImageSourceRef,
        index: usize,
        options: CFDictionaryRef,
    ) -> CFDictionaryRef;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    static kCGColorSpaceDisplayP3: CFStringRef;
    static kCGColorSpaceExtendedLinearSRGB: CFStringRef;
    fn CGImageGetWidth(image: CGImageRef) -> usize;
    fn CGImageGetHeight(image: CGImageRef) -> usize;
    fn CGColorSpaceCreateWithName(name: CFStringRef) -> CGColorSpaceRef;
    fn CGColorSpaceRelease(space: CGColorSpaceRef);
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: CGColorSpaceRef,
        bitmap_info: u32,
    ) -> CGContextRef;
    fn CGContextDrawImage(c: CGContextRef, rect: CGRect, image: CGImageRef);
    fn CGContextRelease(c: CGContextRef);
    fn CGImageRelease(image: CGImageRef);
}

/// Decode the first frame to a `CGImage`, plus ImageIO's display orientation
/// (`kCGImagePropertyOrientation`, 1..=8 — combines the ISOBMFF `irot`/`imir`
/// transform and EXIF, which `CGImageSourceCreateImageAtIndex` does NOT bake in).
/// Caller owns the image and must `CGImageRelease` it. `None` if Image I/O can't
/// open or decode the bytes.
unsafe fn create_cgimage(bytes: &[u8]) -> Option<(CGImageRef, u32)> {
    if bytes.is_empty() {
        return None;
    }
    // CFDataCreate copies the bytes, so the source is self-contained and we can
    // release `data` immediately after.
    let data = CFDataCreate(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize);
    if data.is_null() {
        return None;
    }
    let src = CGImageSourceCreateWithData(data, std::ptr::null());
    if src.is_null() {
        CFRelease(data);
        return None;
    }
    let orientation = source_orientation(src);
    let img = CGImageSourceCreateImageAtIndex(src, 0, std::ptr::null());
    CFRelease(src);
    CFRelease(data);
    if img.is_null() {
        return None;
    }
    Some((img, orientation))
}

/// Read `kCGImagePropertyOrientation` (1..=8) from the source's metadata; 1 (upright)
/// if absent or unreadable.
unsafe fn source_orientation(src: CGImageSourceRef) -> u32 {
    let props = CGImageSourceCopyPropertiesAtIndex(src, 0, std::ptr::null());
    if props.is_null() {
        return 1;
    }
    let mut orient: i32 = 1;
    let val = CFDictionaryGetValue(props, kCGImagePropertyOrientation);
    if !val.is_null() {
        CFNumberGetValue(
            val,
            CF_NUMBER_INT_TYPE,
            &mut orient as *mut i32 as *mut c_void,
        );
    }
    CFRelease(props);
    if (1..=8).contains(&orient) {
        orient as u32
    } else {
        1
    }
}

/// Apply `orientation` (1..=8) to the HDR `Rgba32Float` scratch buffer (16 bytes/px)
/// before it's handed to `finalize_hdr_scrgb` (which assumes upright). Reuses the
/// tested RGBA8 remap via a byte-stride; identity for orientation 1.
fn orient_f32(floats: Vec<f32>, w: u32, h: u32, orientation: u32) -> (Vec<f32>, u32, u32) {
    if orientation <= 1 {
        return (floats, w, h);
    }
    // f32 → u8 is always alignment-safe; the reverse is not (a fresh Vec<u8> may not
    // be 4-byte aligned), so rebuild the floats by copy rather than `cast_slice` back.
    let bytes = bytemuck::cast_slice::<f32, u8>(&floats);
    let (out, ow, oh) = crate::orientation::apply_orientation_bytes(bytes, w, h, orientation, 16);
    let floats = out
        .chunks_exact(4)
        .map(|b| f32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    (floats, ow, oh)
}

/// Sanity bound on a decoded dimension — rejects absurd sizes before allocating.
const MAX_DIM: usize = 100_000;

/// Decode to **Display-P3 8-bit RGBA** (top-down, straight bytes R,G,B,A). The CTM
/// flip makes memory row 0 the top of the image (Quartz draws lower-left-origin).
/// Returns `None` on any failure. `unsafe` because it drives CoreGraphics.
unsafe fn imageio_decode_rgba8_p3(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32, u32)> {
    let (img, orientation) = create_cgimage(bytes)?;
    let result = (|| {
        let w = CGImageGetWidth(img);
        let h = CGImageGetHeight(img);
        if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM {
            return None;
        }
        let stride = w.checked_mul(4)?;
        let len = stride.checked_mul(h)?;
        let space = CGColorSpaceCreateWithName(kCGColorSpaceDisplayP3);
        if space.is_null() {
            return None;
        }
        let mut buf = vec![0u8; len];
        let ctx = CGBitmapContextCreate(
            buf.as_mut_ptr() as *mut c_void,
            w,
            h,
            8,
            stride,
            space,
            ALPHA_PREMULTIPLIED_LAST,
        );
        CGColorSpaceRelease(space);
        if ctx.is_null() {
            return None;
        }
        // No CTM flip: `CGContextDrawImage` into a freshly-created `CGBitmapContext`
        // already lands the image top-down (row 0 = top) in the buffer. (A flip here
        // vertically mirrors the result — verified against Quick Look.) Rotation is
        // applied separately via `kCGImagePropertyOrientation`.
        let rect = CGRect {
            origin: CGPoint { x: 0.0, y: 0.0 },
            size: CGSize {
                width: w as f64,
                height: h as f64,
            },
        };
        CGContextDrawImage(ctx, rect, img);
        CGContextRelease(ctx);
        Some((buf, w as u32, h as u32, orientation))
    })();
    CGImageRelease(img);
    result
}

/// Decode an HDR (PQ/HLG) frame to **extended-linear-sRGB f32** (scene-linear,
/// BT.709 primaries, extended range), top-down. CoreGraphics applies the PQ/HLG
/// decode + gamut conversion; we keep the float values for the HDR output path.
/// `unsafe` because it drives CoreGraphics.
unsafe fn imageio_decode_hdr(bytes: &[u8]) -> Option<(Vec<f32>, u32, u32, u32)> {
    let (img, orientation) = create_cgimage(bytes)?;
    let result = (|| {
        let w = CGImageGetWidth(img);
        let h = CGImageGetHeight(img);
        if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM {
            return None;
        }
        let count = w.checked_mul(h)?.checked_mul(4)?;
        let stride = w.checked_mul(16)?; // 4 channels * f32
        let space = CGColorSpaceCreateWithName(kCGColorSpaceExtendedLinearSRGB);
        if space.is_null() {
            return None;
        }
        let mut floats = vec![0f32; count];
        let info = ALPHA_PREMULTIPLIED_LAST | FLOAT_COMPONENTS | BYTE_ORDER_32_LITTLE;
        let ctx = CGBitmapContextCreate(
            floats.as_mut_ptr() as *mut c_void,
            w,
            h,
            32,
            stride,
            space,
            info,
        );
        CGColorSpaceRelease(space);
        if ctx.is_null() {
            return None;
        }
        // No CTM flip (see the SDR path) — the buffer lands top-down already.
        let rect = CGRect {
            origin: CGPoint { x: 0.0, y: 0.0 },
            size: CGSize {
                width: w as f64,
                height: h as f64,
            },
        };
        CGContextDrawImage(ctx, rect, img);
        CGContextRelease(ctx);
        Some((floats, w as u32, h as u32, orientation))
    })();
    CGImageRelease(img);
    result
}
