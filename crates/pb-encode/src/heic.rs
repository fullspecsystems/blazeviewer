//! **HEIC** through macOS Image I/O (export-plan.md rung 4) — the OS's own HEVC encoder, which
//! Apple licenses; there is no free one to ship elsewhere, so this format is Mac-only. 8-bit
//! with alpha; the colour profile rides on the `CGImage`'s colour space, which Image I/O writes
//! into the file.
//!
//! **Metadata.** Image I/O writes the file's Exif item itself, from property dictionaries
//! (`{TIFF}`, `{Exif}`, `{GPS}`), so the allowlisted block is re-expressed in its terms: text
//! as strings, rationals as numbers (Image I/O turns them back into rationals, approximating
//! any that aren't exact), GPS coordinates as decimal degrees. Only the block's own, already
//! allowlisted tags are passed (measured on macOS 27.2: Image I/O writes back exactly those,
//! adding nothing of its own). A second Copyright string is joined to the first with "; ",
//! since Image I/O takes one.
//!
//! Hand-rolled FFI to a handful of stable CoreFoundation / CoreGraphics / Image I/O C calls,
//! the same posture as `pb-decode/src/imageio.rs`. Every CF object is owned by a [`Cf`] guard.

use std::ffi::{c_char, c_void};

use crate::encoder::{Caps, Encoder};
use crate::types::{ExportImage, OutputColor};
use crate::EncodeError;

type CFTypeRef = *const c_void;

const CF_NUMBER_FLOAT64: isize = 6; // kCFNumberFloat64Type
const CF_NUMBER_INT: isize = 9; // kCFNumberIntType
const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const IMAGE_ALPHA_NONE: u32 = 0; // kCGImageAlphaNone
const IMAGE_ALPHA_LAST: u32 = 3; // kCGImageAlphaLast: straight RGBA
const RENDERING_INTENT_DEFAULT: i32 = 0;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: u8;
    static kCFTypeDictionaryValueCallBacks: u8;
    static kCFTypeArrayCallBacks: u8;
    fn CFRelease(cf: CFTypeRef);
    fn CFArrayCreate(
        alloc: CFTypeRef,
        values: *const CFTypeRef,
        count: isize,
        callbacks: *const c_void,
    ) -> CFTypeRef;
    fn CFStringCreateWithBytes(
        alloc: CFTypeRef,
        bytes: *const u8,
        len: isize,
        encoding: u32,
        external_bom: bool,
    ) -> CFTypeRef;
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
    static kCGImagePropertyTIFFDictionary: CFTypeRef;
    static kCGImagePropertyExifDictionary: CFTypeRef;
    static kCGImagePropertyGPSDictionary: CFTypeRef;
    static kCGImagePropertyTIFFMake: CFTypeRef;
    static kCGImagePropertyTIFFModel: CFTypeRef;
    static kCGImagePropertyTIFFDateTime: CFTypeRef;
    static kCGImagePropertyTIFFArtist: CFTypeRef;
    static kCGImagePropertyTIFFCopyright: CFTypeRef;
    static kCGImagePropertyTIFFOrientation: CFTypeRef;
    static kCGImagePropertyExifDateTimeOriginal: CFTypeRef;
    static kCGImagePropertyExifDateTimeDigitized: CFTypeRef;
    static kCGImagePropertyExifOffsetTime: CFTypeRef;
    static kCGImagePropertyExifOffsetTimeOriginal: CFTypeRef;
    static kCGImagePropertyExifOffsetTimeDigitized: CFTypeRef;
    static kCGImagePropertyExifSubsecTime: CFTypeRef;
    static kCGImagePropertyExifSubsecTimeOriginal: CFTypeRef;
    static kCGImagePropertyExifSubsecTimeDigitized: CFTypeRef;
    static kCGImagePropertyExifLensMake: CFTypeRef;
    static kCGImagePropertyExifLensModel: CFTypeRef;
    static kCGImagePropertyExifLensSpecification: CFTypeRef;
    static kCGImagePropertyExifExposureTime: CFTypeRef;
    static kCGImagePropertyExifFNumber: CFTypeRef;
    static kCGImagePropertyExifApertureValue: CFTypeRef;
    static kCGImagePropertyExifMaxApertureValue: CFTypeRef;
    static kCGImagePropertyExifShutterSpeedValue: CFTypeRef;
    static kCGImagePropertyExifBrightnessValue: CFTypeRef;
    static kCGImagePropertyExifExposureBiasValue: CFTypeRef;
    static kCGImagePropertyExifExposureProgram: CFTypeRef;
    static kCGImagePropertyExifExposureMode: CFTypeRef;
    static kCGImagePropertyExifISOSpeedRatings: CFTypeRef;
    static kCGImagePropertyExifSensitivityType: CFTypeRef;
    static kCGImagePropertyExifMeteringMode: CFTypeRef;
    static kCGImagePropertyExifLightSource: CFTypeRef;
    static kCGImagePropertyExifFlash: CFTypeRef;
    static kCGImagePropertyExifFocalLength: CFTypeRef;
    static kCGImagePropertyExifFocalLenIn35mmFilm: CFTypeRef;
    static kCGImagePropertyExifDigitalZoomRatio: CFTypeRef;
    static kCGImagePropertyExifWhiteBalance: CFTypeRef;
    static kCGImagePropertyExifSceneCaptureType: CFTypeRef;
    static kCGImagePropertyExifContrast: CFTypeRef;
    static kCGImagePropertyExifSaturation: CFTypeRef;
    static kCGImagePropertyExifSharpness: CFTypeRef;
    static kCGImagePropertyGPSVersion: CFTypeRef;
    static kCGImagePropertyGPSLatitudeRef: CFTypeRef;
    static kCGImagePropertyGPSLatitude: CFTypeRef;
    static kCGImagePropertyGPSLongitudeRef: CFTypeRef;
    static kCGImagePropertyGPSLongitude: CFTypeRef;
    static kCGImagePropertyGPSAltitudeRef: CFTypeRef;
    static kCGImagePropertyGPSAltitude: CFTypeRef;
    static kCGImagePropertyGPSTimeStamp: CFTypeRef;
    static kCGImagePropertyGPSDateStamp: CFTypeRef;
    static kCGImagePropertyGPSImgDirectionRef: CFTypeRef;
    static kCGImagePropertyGPSImgDirection: CFTypeRef;
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
    fn writes_metadata(&self) -> bool {
        true
    }
    fn encode(&self, img: &ExportImage) -> Result<Vec<u8>, EncodeError> {
        encode_heic(img, self.quality)
    }
}

/// Which Image I/O dictionary a key lives in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dict {
    Tiff,
    Exif,
    Gps,
}

/// How a field's value is expressed in Image I/O's dictionaries.
#[derive(Clone, Copy)]
enum As {
    /// A string (each ASCII part, joined).
    Text,
    /// An integer (the first value).
    Int,
    /// An array of integers.
    Ints,
    /// A real number (the first rational).
    Real,
    /// An array of reals.
    Reals,
    /// Degrees, minutes, seconds → decimal degrees.
    Degrees,
    /// Hours, minutes, seconds → `HH:MM:SS.ss`.
    Clock,
}

/// The Image I/O key for an allowlisted EXIF tag (`None`: not passed on).
fn key_for(tag: exif::Tag) -> Option<(Dict, CFTypeRef, As)> {
    use exif::Tag as T;
    // SAFETY: reading Image I/O's constant key strings, which are initialized at load.
    unsafe {
        Some(match tag {
            T::Make => (Dict::Tiff, kCGImagePropertyTIFFMake, As::Text),
            T::Model => (Dict::Tiff, kCGImagePropertyTIFFModel, As::Text),
            T::DateTime => (Dict::Tiff, kCGImagePropertyTIFFDateTime, As::Text),
            T::Artist => (Dict::Tiff, kCGImagePropertyTIFFArtist, As::Text),
            T::Copyright => (Dict::Tiff, kCGImagePropertyTIFFCopyright, As::Text),
            T::Orientation => (Dict::Tiff, kCGImagePropertyTIFFOrientation, As::Int),
            T::DateTimeOriginal => (Dict::Exif, kCGImagePropertyExifDateTimeOriginal, As::Text),
            T::DateTimeDigitized => (Dict::Exif, kCGImagePropertyExifDateTimeDigitized, As::Text),
            T::OffsetTime => (Dict::Exif, kCGImagePropertyExifOffsetTime, As::Text),
            T::OffsetTimeOriginal => (Dict::Exif, kCGImagePropertyExifOffsetTimeOriginal, As::Text),
            T::OffsetTimeDigitized => (
                Dict::Exif,
                kCGImagePropertyExifOffsetTimeDigitized,
                As::Text,
            ),
            T::SubSecTime => (Dict::Exif, kCGImagePropertyExifSubsecTime, As::Text),
            T::SubSecTimeOriginal => (Dict::Exif, kCGImagePropertyExifSubsecTimeOriginal, As::Text),
            T::SubSecTimeDigitized => (
                Dict::Exif,
                kCGImagePropertyExifSubsecTimeDigitized,
                As::Text,
            ),
            T::LensMake => (Dict::Exif, kCGImagePropertyExifLensMake, As::Text),
            T::LensModel => (Dict::Exif, kCGImagePropertyExifLensModel, As::Text),
            T::LensSpecification => (Dict::Exif, kCGImagePropertyExifLensSpecification, As::Reals),
            T::ExposureTime => (Dict::Exif, kCGImagePropertyExifExposureTime, As::Real),
            T::FNumber => (Dict::Exif, kCGImagePropertyExifFNumber, As::Real),
            T::ApertureValue => (Dict::Exif, kCGImagePropertyExifApertureValue, As::Real),
            T::MaxApertureValue => (Dict::Exif, kCGImagePropertyExifMaxApertureValue, As::Real),
            T::ShutterSpeedValue => (Dict::Exif, kCGImagePropertyExifShutterSpeedValue, As::Real),
            T::BrightnessValue => (Dict::Exif, kCGImagePropertyExifBrightnessValue, As::Real),
            T::ExposureBiasValue => (Dict::Exif, kCGImagePropertyExifExposureBiasValue, As::Real),
            T::ExposureProgram => (Dict::Exif, kCGImagePropertyExifExposureProgram, As::Int),
            T::ExposureMode => (Dict::Exif, kCGImagePropertyExifExposureMode, As::Int),
            T::PhotographicSensitivity => {
                (Dict::Exif, kCGImagePropertyExifISOSpeedRatings, As::Ints)
            }
            T::SensitivityType => (Dict::Exif, kCGImagePropertyExifSensitivityType, As::Int),
            T::MeteringMode => (Dict::Exif, kCGImagePropertyExifMeteringMode, As::Int),
            T::LightSource => (Dict::Exif, kCGImagePropertyExifLightSource, As::Int),
            T::Flash => (Dict::Exif, kCGImagePropertyExifFlash, As::Int),
            T::FocalLength => (Dict::Exif, kCGImagePropertyExifFocalLength, As::Real),
            T::FocalLengthIn35mmFilm => {
                (Dict::Exif, kCGImagePropertyExifFocalLenIn35mmFilm, As::Int)
            }
            T::DigitalZoomRatio => (Dict::Exif, kCGImagePropertyExifDigitalZoomRatio, As::Real),
            T::WhiteBalance => (Dict::Exif, kCGImagePropertyExifWhiteBalance, As::Int),
            T::SceneCaptureType => (Dict::Exif, kCGImagePropertyExifSceneCaptureType, As::Int),
            T::Contrast => (Dict::Exif, kCGImagePropertyExifContrast, As::Int),
            T::Saturation => (Dict::Exif, kCGImagePropertyExifSaturation, As::Int),
            T::Sharpness => (Dict::Exif, kCGImagePropertyExifSharpness, As::Int),
            T::GPSVersionID => (Dict::Gps, kCGImagePropertyGPSVersion, As::Ints),
            T::GPSLatitudeRef => (Dict::Gps, kCGImagePropertyGPSLatitudeRef, As::Text),
            T::GPSLatitude => (Dict::Gps, kCGImagePropertyGPSLatitude, As::Degrees),
            T::GPSLongitudeRef => (Dict::Gps, kCGImagePropertyGPSLongitudeRef, As::Text),
            T::GPSLongitude => (Dict::Gps, kCGImagePropertyGPSLongitude, As::Degrees),
            T::GPSAltitudeRef => (Dict::Gps, kCGImagePropertyGPSAltitudeRef, As::Int),
            T::GPSAltitude => (Dict::Gps, kCGImagePropertyGPSAltitude, As::Real),
            T::GPSTimeStamp => (Dict::Gps, kCGImagePropertyGPSTimeStamp, As::Clock),
            T::GPSDateStamp => (Dict::Gps, kCGImagePropertyGPSDateStamp, As::Text),
            T::GPSImgDirectionRef => (Dict::Gps, kCGImagePropertyGPSImgDirectionRef, As::Text),
            T::GPSImgDirection => (Dict::Gps, kCGImagePropertyGPSImgDirection, As::Real),
            _ => return None,
        })
    }
}

fn cf_string(s: &str) -> Result<Cf, EncodeError> {
    Cf::new(
        unsafe {
            CFStringCreateWithBytes(
                std::ptr::null(),
                s.as_ptr(),
                s.len() as isize,
                CF_STRING_ENCODING_UTF8,
                false,
            )
        },
        "out of memory",
    )
}

fn cf_int(v: i32) -> Result<Cf, EncodeError> {
    Cf::new(
        unsafe { CFNumberCreate(std::ptr::null(), CF_NUMBER_INT, (&v as *const i32).cast()) },
        "out of memory",
    )
}

fn cf_real(v: f64) -> Result<Cf, EncodeError> {
    Cf::new(
        unsafe {
            CFNumberCreate(
                std::ptr::null(),
                CF_NUMBER_FLOAT64,
                (&v as *const f64).cast(),
            )
        },
        "out of memory",
    )
}

fn cf_array(items: &[Cf]) -> Result<Cf, EncodeError> {
    let refs: Vec<CFTypeRef> = items.iter().map(|c| c.0).collect();
    Cf::new(
        unsafe {
            CFArrayCreate(
                std::ptr::null(),
                refs.as_ptr(),
                refs.len() as isize,
                (&raw const kCFTypeArrayCallBacks).cast(),
            )
        },
        "out of memory",
    )
}

/// A dictionary of borrowed keys (Image I/O's static constants) to owned values; the
/// dictionary retains the values, so the guards can go once it exists.
fn cf_dict(entries: &[(CFTypeRef, Cf)]) -> Result<Cf, EncodeError> {
    let keys: Vec<CFTypeRef> = entries.iter().map(|(k, _)| *k).collect();
    let values: Vec<CFTypeRef> = entries.iter().map(|(_, v)| v.0).collect();
    Cf::new(
        unsafe {
            CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                keys.len() as isize,
                (&raw const kCFTypeDictionaryKeyCallBacks).cast(),
                (&raw const kCFTypeDictionaryValueCallBacks).cast(),
            )
        },
        "out of memory",
    )
}

/// One field's value in Image I/O's terms (`None` when it doesn't have the expected shape).
fn cf_value(value: &exif::Value, how: As) -> Result<Option<Cf>, EncodeError> {
    let ints: Vec<i64> = match value {
        exif::Value::Byte(v) => v.iter().map(|&x| x as i64).collect(),
        exif::Value::Short(v) => v.iter().map(|&x| x as i64).collect(),
        exif::Value::Long(v) => v.iter().map(|&x| x as i64).collect(),
        _ => Vec::new(),
    };
    let reals: Vec<f64> = match value {
        exif::Value::Rational(v) => v
            .iter()
            .map(|r| {
                if r.denom == 0 {
                    0.0
                } else {
                    r.num as f64 / r.denom as f64
                }
            })
            .collect(),
        exif::Value::SRational(v) => v
            .iter()
            .map(|r| {
                if r.denom == 0 {
                    0.0
                } else {
                    r.num as f64 / r.denom as f64
                }
            })
            .collect(),
        _ => Vec::new(),
    };
    Ok(Some(match how {
        As::Text => {
            let exif::Value::Ascii(parts) = value else {
                return Ok(None);
            };
            let text = parts
                .iter()
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .collect::<Vec<_>>()
                .join("; ");
            cf_string(&text)?
        }
        As::Int => match ints.first() {
            Some(&v) => cf_int(v as i32)?,
            None => return Ok(None),
        },
        As::Ints if !ints.is_empty() => cf_array(
            &ints
                .iter()
                .map(|&v| cf_int(v as i32))
                .collect::<Result<Vec<_>, _>>()?,
        )?,
        As::Real => match reals.first() {
            Some(&v) => cf_real(v)?,
            None => return Ok(None),
        },
        As::Reals if !reals.is_empty() => cf_array(
            &reals
                .iter()
                .map(|&v| cf_real(v))
                .collect::<Result<Vec<_>, _>>()?,
        )?,
        As::Degrees if reals.len() == 3 => cf_real(reals[0] + reals[1] / 60.0 + reals[2] / 3600.0)?,
        As::Clock if reals.len() == 3 => cf_string(&format!(
            "{:02}:{:02}:{:05.2}",
            reals[0] as u32, reals[1] as u32, reals[2]
        ))?,
        _ => return Ok(None),
    }))
}

/// The `{TIFF}` / `{Exif}` / `{GPS}` dictionaries for an EXIF block, as top-level entries.
fn metadata_dicts(block: &[u8]) -> Result<Vec<(CFTypeRef, Cf)>, EncodeError> {
    let mut by_dict: [(Dict, Vec<(CFTypeRef, Cf)>); 3] = [
        (Dict::Tiff, Vec::new()),
        (Dict::Exif, Vec::new()),
        (Dict::Gps, Vec::new()),
    ];
    for f in crate::metadata::fields(block) {
        let Some((dict, key, how)) = key_for(f.tag) else {
            continue;
        };
        if let Some(v) = cf_value(&f.value, how)? {
            if let Some((_, entries)) = by_dict.iter_mut().find(|(d, _)| *d == dict) {
                entries.push((key, v));
            }
        }
    }
    let mut out = Vec::new();
    for (dict, entries) in by_dict {
        if entries.is_empty() {
            continue;
        }
        // SAFETY: Image I/O's constant key strings.
        let key = unsafe {
            match dict {
                Dict::Tiff => kCGImagePropertyTIFFDictionary,
                Dict::Exif => kCGImagePropertyExifDictionary,
                Dict::Gps => kCGImagePropertyGPSDictionary,
            }
        };
        out.push((key, cf_dict(&entries)?));
    }
    Ok(out)
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
        let mut entries = vec![(kCGImageDestinationLossyCompressionQuality, number)];
        if let Some(block) = &img.metadata.exif {
            entries.extend(metadata_dicts(block)?);
        }
        let props = cf_dict(&entries)?;
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
            metadata: Default::default(),
        }
    }

    /// Required test 2 for HEIC: Image I/O writes the chosen tags into the file's Exif item,
    /// which an EXIF reader finds again, with none of the private ones (they were never
    /// passed) and Orientation 1.
    #[test]
    fn metadata_round_trips_through_image_io() {
        use crate::metadata::tests::{dirty_source, has, value};
        use crate::metadata::MetadataChoice;
        let mut img = image(
            Channels::Rgb,
            vec![120; 64 * 48 * 3],
            OutputColor::Icc(crate::icc::display_p3_profile()),
        );
        img.metadata = dirty_source().output(MetadataChoice::ALL);
        let heic = encode_heic(&img, 80).unwrap();
        let exif = exif::Reader::new()
            .read_from_container(&mut std::io::Cursor::new(&heic))
            .expect("an Exif item");
        let tags: Vec<(exif::Tag, String)> = exif
            .fields()
            .filter(|f| f.ifd_num == exif::In::PRIMARY)
            .map(|f| (f.tag, f.display_value().to_string()))
            .collect();
        assert_eq!(value(&tags, exif::Tag::Make), Some("\"Canon\""), "{tags:?}");
        assert_eq!(value(&tags, exif::Tag::Model), Some("\"Canon EOS R5\""));
        assert_eq!(value(&tags, exif::Tag::Artist), Some("\"Jane Doe\""));
        assert_eq!(
            value(&tags, exif::Tag::DateTimeOriginal),
            Some("2024-05-06 07:08:09")
        );
        assert_eq!(value(&tags, exif::Tag::FNumber), Some("2.8"));
        assert_eq!(value(&tags, exif::Tag::GPSLatitudeRef), Some("N"));
        assert!(has(&tags, exif::Tag::GPSLatitude) && has(&tags, exif::Tag::GPSLongitude));
        assert_eq!(
            value(&tags, exif::Tag::Orientation),
            Some("row 0 at top and column 0 at left")
        );
        for private in [
            exif::Tag::MakerNote,
            exif::Tag::BodySerialNumber,
            exif::Tag::UserComment,
            exif::Tag::ImageDescription,
            exif::Tag::CameraOwnerName,
        ] {
            assert!(!has(&tags, private), "{private} leaked");
        }
        // No metadata: no Exif item at all.
        img.metadata = Default::default();
        let plain = encode_heic(&img, 80).unwrap();
        assert!(exif::Reader::new()
            .read_from_container(&mut std::io::Cursor::new(&plain))
            .is_err());
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
