//! **HEIC** through macOS Image I/O (export-plan.md rung 4) — the OS's own HEVC encoder, which
//! Apple licenses; there is no free one to ship elsewhere, so this format is Mac-only. 8-bit
//! with alpha; the colour profile rides on the `CGImage`'s colour space, which Image I/O writes
//! into the file.
//!
//! Hand-rolled FFI to a handful of stable CoreFoundation / CoreGraphics / Image I/O C calls,
//! the same posture as `pb-decode/src/imageio.rs`. Every CF object is owned by a [`Cf`] guard.

use std::ffi::{c_char, c_void};

use crate::encoder::{Caps, Encoder};
use crate::types::{ExportImage, OutputColor};
use crate::EncodeError;

type CFTypeRef = *const c_void;

const CF_NUMBER_FLOAT64: isize = 6; // kCFNumberFloat64Type
const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const IMAGE_ALPHA_NONE: u32 = 0; // kCGImageAlphaNone
const IMAGE_ALPHA_LAST: u32 = 3; // kCGImageAlphaLast: straight RGBA
const RENDERING_INTENT_DEFAULT: i32 = 0;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: u8;
    static kCFTypeDictionaryValueCallBacks: u8;
    fn CFRelease(cf: CFTypeRef);
    fn CFDataCreate(alloc: CFTypeRef, bytes: *const u8, len: isize) -> CFTypeRef;
    fn CFDataCreateMutable(alloc: CFTypeRef, capacity: isize) -> CFTypeRef;
    fn CFDataGetLength(data: CFTypeRef) -> isize;
    fn CFDataGetBytePtr(data: CFTypeRef) -> *const u8;
    fn CFNumberCreate(alloc: CFTypeRef, the_type: isize, value: *const c_void) -> CFTypeRef;
    fn CFStringCreateWithCString(alloc: CFTypeRef, s: *const c_char, encoding: u32) -> CFTypeRef;
    fn CFDictionaryCreate(
        alloc: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFTypeRef;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    static kCGColorSpaceSRGB: CFTypeRef;
    fn CGColorSpaceCreateWithName(name: CFTypeRef) -> CFTypeRef;
    fn CGColorSpaceCreateWithICCData(data: CFTypeRef) -> CFTypeRef;
    fn CGDataProviderCreateWithData(
        info: *mut c_void,
        data: *const c_void,
        size: usize,
        release: Option<unsafe extern "C" fn(*mut c_void, *const c_void, usize)>,
    ) -> CFTypeRef;
    #[allow(clippy::too_many_arguments)]
    fn CGImageCreate(
        width: usize,
        height: usize,
        bits_per_component: usize,
        bits_per_pixel: usize,
        bytes_per_row: usize,
        space: CFTypeRef,
        bitmap_info: u32,
        provider: CFTypeRef,
        decode: *const f64,
        should_interpolate: bool,
        intent: i32,
    ) -> CFTypeRef;
}

#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    static kCGImageDestinationLossyCompressionQuality: CFTypeRef;
    fn CGImageDestinationCreateWithData(
        data: CFTypeRef,
        uti: CFTypeRef,
        count: usize,
        options: CFTypeRef,
    ) -> CFTypeRef;
    fn CGImageDestinationAddImage(dest: CFTypeRef, image: CFTypeRef, properties: CFTypeRef);
    fn CGImageDestinationFinalize(dest: CFTypeRef) -> bool;
}

/// An owned CoreFoundation object, released on drop.
struct Cf(CFTypeRef);

impl Cf {
    fn new(r: CFTypeRef, what: &str) -> Result<Cf, EncodeError> {
        if r.is_null() {
            Err(EncodeError::Codec(format!("heic: {what}")))
        } else {
            Ok(Cf(r))
        }
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) }
    }
}

/// The HEIC export format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeicEncoder {
    /// 1–100 → Image I/O's 0–1 lossy quality.
    pub quality: u8,
}

impl Encoder for HeicEncoder {
    fn caps(&self) -> Caps {
        Caps::EIGHT_BIT_ALPHA
    }
    fn extension(&self) -> &'static str {
        "heic"
    }
    fn lossless(&self) -> bool {
        false
    }
    fn encode(&self, img: &ExportImage) -> Result<Vec<u8>, EncodeError> {
        encode_heic(img, self.quality)
    }
}

/// Encode a prepared 8-bit image as HEIC.
pub fn encode_heic(img: &ExportImage, quality: u8) -> Result<Vec<u8>, EncodeError> {
    let (px, alpha) = crate::px::rgb8(img)?;
    let (w, h) = (img.width as usize, img.height as usize);
    let n = if alpha { 4 } else { 3 };
    unsafe {
        let space = match &img.color {
            OutputColor::Srgb => Cf::new(CGColorSpaceCreateWithName(kCGColorSpaceSRGB), "no sRGB")?,
            OutputColor::Icc(p) => {
                let data = Cf::new(
                    CFDataCreate(std::ptr::null(), p.as_ptr(), p.len() as isize),
                    "out of memory",
                )?;
                Cf::new(
                    CGColorSpaceCreateWithICCData(data.0),
                    "couldn't use the color profile",
                )?
            }
        };
        // The provider borrows `px`, which outlives every use: the destination is finalized
        // (and everything released) before this function returns.
        let provider = Cf::new(
            CGDataProviderCreateWithData(std::ptr::null_mut(), px.as_ptr().cast(), px.len(), None),
            "out of memory",
        )?;
        let image = Cf::new(
            CGImageCreate(
                w,
                h,
                8,
                8 * n,
                w * n,
                space.0,
                if alpha {
                    IMAGE_ALPHA_LAST
                } else {
                    IMAGE_ALPHA_NONE
                },
                provider.0,
                std::ptr::null(),
                false,
                RENDERING_INTENT_DEFAULT,
            ),
            "couldn't take the pixels",
        )?;

        let out = Cf::new(CFDataCreateMutable(std::ptr::null(), 0), "out of memory")?;
        let uti = Cf::new(
            CFStringCreateWithCString(
                std::ptr::null(),
                c"public.heic".as_ptr(),
                CF_STRING_ENCODING_UTF8,
            ),
            "out of memory",
        )?;
        let dest = Cf::new(
            CGImageDestinationCreateWithData(out.0, uti.0, 1, std::ptr::null()),
            "this Mac can't write HEIC",
        )?;
        let q = quality.clamp(1, 100) as f64 / 100.0;
        let number = Cf::new(
            CFNumberCreate(
                std::ptr::null(),
                CF_NUMBER_FLOAT64,
                (&q as *const f64).cast(),
            ),
            "out of memory",
        )?;
        let keys = [kCGImageDestinationLossyCompressionQuality];
        let values = [number.0];
        let props = Cf::new(
            CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                1,
                (&raw const kCFTypeDictionaryKeyCallBacks).cast(),
                (&raw const kCFTypeDictionaryValueCallBacks).cast(),
            ),
            "out of memory",
        )?;
        CGImageDestinationAddImage(dest.0, image.0, props.0);
        if !CGImageDestinationFinalize(dest.0) {
            return Err(EncodeError::Codec(
                "heic: Image I/O couldn't write the file".into(),
            ));
        }
        let len = CFDataGetLength(out.0) as usize;
        let ptr = CFDataGetBytePtr(out.0);
        if ptr.is_null() || len == 0 {
            return Err(EncodeError::Codec("heic: empty file".into()));
        }
        Ok(std::slice::from_raw_parts(ptr, len).to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Channels, Samples};

    fn image(channels: Channels, px: Vec<u8>, color: OutputColor) -> ExportImage {
        ExportImage {
            width: 64,
            height: 48,
            channels,
            samples: Samples::U8(px),
            color,
            notes: Vec::new(),
        }
    }

    /// A HEIF file with the HEIC brand, smaller at lower quality. (pb-decode's tests read the
    /// pixels and colour back through Image I/O, as the viewer does.)
    #[test]
    fn writes_heic_and_quality_trades_size() {
        let px: Vec<u8> = (0..64 * 48)
            .flat_map(|i| [(i % 64 * 4) as u8, (i / 64 * 5) as u8, 60, 255])
            .collect();
        let src = image(
            Channels::Rgba,
            px,
            OutputColor::Icc(crate::icc::display_p3_profile()),
        );
        let hi = encode_heic(&src, 95).unwrap();
        let lo = encode_heic(&src, 10).unwrap();
        assert_eq!(&hi[4..12], b"ftypheic");
        assert!(lo.len() < hi.len(), "{} < {}", lo.len(), hi.len());
        let rgb = image(Channels::Rgb, vec![200; 64 * 48 * 3], OutputColor::Srgb);
        assert!(encode_heic(&rgb, 80).is_ok());
    }

    #[test]
    fn refuses_deep_samples() {
        let deep = ExportImage {
            samples: Samples::U16(vec![0; 64 * 48 * 3]),
            ..image(Channels::Rgb, vec![], OutputColor::Srgb)
        };
        assert_eq!(encode_heic(&deep, 80), Err(EncodeError::Malformed));
    }
}
