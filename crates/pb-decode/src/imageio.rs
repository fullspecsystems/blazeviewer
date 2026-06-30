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
/// `kCFNumberFloat64Type` — read a CFNumber into a C `double` (f64). Used for the
/// per-frame delay times, which Image I/O reports in seconds.
const CF_NUMBER_FLOAT64_TYPE: isize = 6;

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
    // Per-container format dictionaries + their per-frame delay / loop-count keys.
    // Image I/O reports delays in seconds (the Unclamped variant is the true value;
    // the plain one is floored to 0.1 s) and loop counts as ints (0 = infinite).
    static kCGImagePropertyGIFDictionary: CFStringRef;
    static kCGImagePropertyGIFUnclampedDelayTime: CFStringRef;
    static kCGImagePropertyGIFDelayTime: CFStringRef;
    static kCGImagePropertyGIFLoopCount: CFStringRef;
    static kCGImagePropertyPNGDictionary: CFStringRef;
    static kCGImagePropertyAPNGUnclampedDelayTime: CFStringRef;
    static kCGImagePropertyAPNGDelayTime: CFStringRef;
    static kCGImagePropertyAPNGLoopCount: CFStringRef;
    static kCGImagePropertyWebPDictionary: CFStringRef;
    static kCGImagePropertyWebPUnclampedDelayTime: CFStringRef;
    static kCGImagePropertyWebPDelayTime: CFStringRef;
    static kCGImagePropertyWebPLoopCount: CFStringRef;
    static kCGImagePropertyHEICSDictionary: CFStringRef;
    static kCGImagePropertyHEICSUnclampedDelayTime: CFStringRef;
    static kCGImagePropertyHEICSDelayTime: CFStringRef;
    static kCGImagePropertyHEICSLoopCount: CFStringRef;
    fn CGImageSourceCreateWithData(data: CFDataRef, options: CFDictionaryRef) -> CGImageSourceRef;
    fn CGImageSourceGetCount(isrc: CGImageSourceRef) -> usize;
    fn CGImageSourceCreateImageAtIndex(
        isrc: CGImageSourceRef,
        index: usize,
        options: CFDictionaryRef,
    ) -> CGImageRef;
    fn CGImageSourceCopyProperties(
        isrc: CGImageSourceRef,
        options: CFDictionaryRef,
    ) -> CFDictionaryRef;
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

/// Draw a `CGImage` into a fresh **Display-P3 8-bit RGBA** bitmap (top-down, row 0 =
/// top). Returns the buffer + dimensions, or `None` on any failure. The bytes are
/// **premultiplied** (Quartz contexts require it); callers that need straight alpha
/// (the animation path) un-premultiply afterward — the still path's HEIC sources are
/// opaque, so it leaves them as-is. `unsafe` because it drives CoreGraphics.
unsafe fn draw_cgimage_p3(img: CGImageRef) -> Option<(Vec<u8>, u32, u32)> {
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
    Some((buf, w as u32, h as u32))
}

/// Decode to **Display-P3 8-bit RGBA** (top-down), plus the source orientation.
/// Returns `None` on any failure. `unsafe` because it drives CoreGraphics.
unsafe fn imageio_decode_rgba8_p3(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32, u32)> {
    let (img, orientation) = create_cgimage(bytes)?;
    let result = draw_cgimage_p3(img).map(|(buf, w, h)| (buf, w, h, orientation));
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

// --- Multi-frame (animation) decode ------------------------------------------------
//
// `CGImageSource` decodes an animated GIF/APNG/WebP — *and* an AVIF/HEIC image
// sequence — as N fully-composited frames (it does the dispose/blend), with per-frame
// delays and a loop count in the format property dictionaries. This is the macOS
// backend `pb_decode::animation` prefers (the only one that can do AV1/HEVC
// sequences). Frames come back **Display-P3, straight-alpha** RGBA8; the caller
// downscales-to-fit and carries the P3→BT.709 transform, like the still HEIC path.

/// One composited animation frame from Image I/O: P3, straight-alpha RGBA8, plus the
/// frame's display duration in seconds (the caller normalizes/clamps it).
pub(crate) struct ImageIoFrame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub delay_secs: f64,
}

/// A decoded image sequence from Image I/O: its frames and the loop count
/// (`0` = infinite, matching every container's convention).
pub(crate) struct ImageIoAnimation {
    pub frames: Vec<ImageIoFrame>,
    pub loop_count: u32,
}

/// Decode up to `max_frames` composited frames of an animated image via Image I/O.
/// `None` if Image I/O can't open it or it has no frames. Panics never escape (no
/// Rust panics here; the FFI either succeeds or returns null, which we handle).
pub(crate) fn decode_animation_frames(bytes: &[u8], max_frames: usize) -> Option<ImageIoAnimation> {
    unsafe { decode_animation_frames_inner(bytes, max_frames) }
}

unsafe fn decode_animation_frames_inner(
    bytes: &[u8],
    max_frames: usize,
) -> Option<ImageIoAnimation> {
    if bytes.is_empty() || max_frames == 0 {
        return None;
    }
    let data = CFDataCreate(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize);
    if data.is_null() {
        return None;
    }
    let src = CGImageSourceCreateWithData(data, std::ptr::null());
    if src.is_null() {
        CFRelease(data);
        return None;
    }
    let result = (|| {
        let count = CGImageSourceGetCount(src);
        if count == 0 {
            return None;
        }
        // Orientation is read once from the container and applied to every frame
        // (sequences are uniformly oriented); usually 1 (a no-op) for GIF/APNG/WebP.
        let orientation = source_orientation(src);
        let loop_count = {
            let cprops = CGImageSourceCopyProperties(src, std::ptr::null());
            let lc = if cprops.is_null() {
                0
            } else {
                container_loop_count(cprops)
            };
            if !cprops.is_null() {
                CFRelease(cprops);
            }
            lc
        };

        let mut frames = Vec::new();
        for i in 0..count.min(max_frames) {
            let img = CGImageSourceCreateImageAtIndex(src, i, std::ptr::null());
            if img.is_null() {
                continue;
            }
            let drawn = draw_cgimage_p3(img);
            CGImageRelease(img);
            let Some((mut buf, w, h)) = drawn else {
                continue;
            };
            // Quartz drew premultiplied; the renderer wants straight alpha (and these
            // formats routinely carry transparency, unlike the opaque still HEICs).
            unpremultiply(&mut buf);
            let (rgba, fw, fh) = if orientation > 1 {
                crate::orientation::apply_orientation_bytes(&buf, w, h, orientation, 4)
            } else {
                (buf, w, h)
            };
            // Per-frame delay (seconds) from whichever format dictionary applies.
            let pprops = CGImageSourceCopyPropertiesAtIndex(src, i, std::ptr::null());
            let delay_secs = if pprops.is_null() {
                0.0
            } else {
                frame_delay_seconds(pprops).unwrap_or(0.0)
            };
            if !pprops.is_null() {
                CFRelease(pprops);
            }
            frames.push(ImageIoFrame {
                rgba,
                width: fw,
                height: fh,
                delay_secs,
            });
        }
        if frames.is_empty() {
            return None;
        }
        Some(ImageIoAnimation { frames, loop_count })
    })();
    CFRelease(src);
    CFRelease(data);
    result
}

/// Convert premultiplied RGBA bytes (what Quartz produces) back to straight alpha.
/// Opaque pixels (`a == 255`) are untouched; fully transparent ones are zeroed.
fn unpremultiply(buf: &mut [u8]) {
    for px in buf.chunks_exact_mut(4) {
        let a = px[3];
        if a == 0 {
            px[0] = 0;
            px[1] = 0;
            px[2] = 0;
        } else if a < 255 {
            let a = a as u16;
            for c in &mut px[0..3] {
                *c = ((*c as u16 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

/// The known animated-container property dictionaries, each paired with its
/// per-frame Unclamped/clamped delay keys and its loop-count key. We try them in
/// turn since a given file populates exactly one.
unsafe fn format_dicts() -> [(CFStringRef, CFStringRef, CFStringRef, CFStringRef); 4] {
    [
        (
            kCGImagePropertyGIFDictionary,
            kCGImagePropertyGIFUnclampedDelayTime,
            kCGImagePropertyGIFDelayTime,
            kCGImagePropertyGIFLoopCount,
        ),
        (
            kCGImagePropertyPNGDictionary,
            kCGImagePropertyAPNGUnclampedDelayTime,
            kCGImagePropertyAPNGDelayTime,
            kCGImagePropertyAPNGLoopCount,
        ),
        (
            kCGImagePropertyWebPDictionary,
            kCGImagePropertyWebPUnclampedDelayTime,
            kCGImagePropertyWebPDelayTime,
            kCGImagePropertyWebPLoopCount,
        ),
        (
            kCGImagePropertyHEICSDictionary,
            kCGImagePropertyHEICSUnclampedDelayTime,
            kCGImagePropertyHEICSDelayTime,
            kCGImagePropertyHEICSLoopCount,
        ),
    ]
}

/// A frame's delay (seconds) from its per-index properties — the Unclamped value
/// (the encoder's true intent) when present, else the floored one. `None` if no
/// known animated dictionary carries a delay (e.g. an AVIF sequence with none).
unsafe fn frame_delay_seconds(props: CFDictionaryRef) -> Option<f64> {
    for (dict_key, unclamped, clamped, _loop) in format_dicts() {
        let sub = CFDictionaryGetValue(props, dict_key);
        if sub.is_null() {
            continue;
        }
        if let Some(d) =
            cf_dict_get_double(sub, unclamped).or_else(|| cf_dict_get_double(sub, clamped))
        {
            return Some(d);
        }
    }
    None
}

/// The container loop count (`0` = infinite) from whichever animated dictionary the
/// container-level properties carry; `0` if none (treat as loop forever).
unsafe fn container_loop_count(props: CFDictionaryRef) -> u32 {
    for (dict_key, _u, _c, loop_key) in format_dicts() {
        let sub = CFDictionaryGetValue(props, dict_key);
        if sub.is_null() {
            continue;
        }
        if let Some(n) = cf_dict_get_int(sub, loop_key) {
            return n.max(0) as u32;
        }
    }
    0
}

/// Read a CFNumber value out of a CFDictionary as `f64`. `None` if the key is absent
/// or the value isn't a number.
unsafe fn cf_dict_get_double(dict: CFDictionaryRef, key: CFStringRef) -> Option<f64> {
    if dict.is_null() || key.is_null() {
        return None;
    }
    let v = CFDictionaryGetValue(dict, key);
    if v.is_null() {
        return None;
    }
    let mut out: f64 = 0.0;
    CFNumberGetValue(
        v,
        CF_NUMBER_FLOAT64_TYPE,
        &mut out as *mut f64 as *mut c_void,
    )
    .then_some(out)
}

/// Read a CFNumber value out of a CFDictionary as `i32`. `None` if the key is absent
/// or the value isn't a number.
unsafe fn cf_dict_get_int(dict: CFDictionaryRef, key: CFStringRef) -> Option<i32> {
    if dict.is_null() || key.is_null() {
        return None;
    }
    let v = CFDictionaryGetValue(dict, key);
    if v.is_null() {
        return None;
    }
    let mut out: i32 = 0;
    CFNumberGetValue(v, CF_NUMBER_INT_TYPE, &mut out as *mut i32 as *mut c_void).then_some(out)
}
