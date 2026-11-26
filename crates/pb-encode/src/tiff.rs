//! Lossless **TIFF** over the `tiff` crate (export-plan.md rung 4) — for print and for editing
//! in another app. 8- or 16-bit, gray or RGB; alpha as an unassociated extra sample; the colour
//! profile in the ICC tag (an sRGB image embeds none, as JPEG does). LZW with the horizontal
//! predictor: the compression Photoshop and every TIFF reader handle.
//!
//! **Metadata** is native here rather than a block: the EXIF's IFD0 tags (make, model, date,
//! artist, copyright, orientation) go in the image's own IFD0, and its Exif and GPS sub-IFDs
//! are written as real sub-directories the IFD0 points at, the layout a camera's TIFF has.

use std::io::{Cursor, Seek, Write};

use tiff::encoder::colortype::{self, ColorType};
use tiff::encoder::{Compression, DirectoryEncoder, Predictor, TiffKind, TiffValue};
use tiff::tags::{Tag, Type};

use crate::encoder::{Caps, Encoder};
use crate::types::{Channels, ExportImage, OutputColor, Samples};
use crate::EncodeError;

/// The TIFF export format: everything `prepare` produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TiffEncoder;

impl Encoder for TiffEncoder {
    fn caps(&self) -> Caps {
        Caps::ALL
    }
    fn extension(&self) -> &'static str {
        "tif"
    }
    fn lossless(&self) -> bool {
        true
    }
    fn writes_metadata(&self) -> bool {
        true
    }
    fn encode(&self, img: &ExportImage) -> Result<Vec<u8>, EncodeError> {
        encode_tiff(img)
    }
}

/// An ASCII value as stored: the strings, each NUL-terminated (the crate's `str` takes one
/// string, and only plain ASCII).
struct Ascii(Vec<u8>);

impl TiffValue for Ascii {
    const BYTE_LEN: u8 = 1;
    const FIELD_TYPE: Type = Type::ASCII;
    fn count(&self) -> usize {
        self.0.len()
    }
    fn data(&self) -> std::borrow::Cow<'_, [u8]> {
        std::borrow::Cow::Borrowed(&self.0)
    }
}

/// A list of RATIONALs (the crate has a single one only), in the writer's native byte order.
struct Rationals(Vec<u8>);

impl TiffValue for Rationals {
    const BYTE_LEN: u8 = 8;
    const FIELD_TYPE: Type = Type::RATIONAL;
    fn count(&self) -> usize {
        self.0.len() / 8
    }
    fn data(&self) -> std::borrow::Cow<'_, [u8]> {
        std::borrow::Cow::Borrowed(&self.0)
    }
}

/// A list of SRATIONALs, in the writer's native byte order.
struct SRationals(Vec<u8>);

impl TiffValue for SRationals {
    const BYTE_LEN: u8 = 8;
    const FIELD_TYPE: Type = Type::SRATIONAL;
    fn count(&self) -> usize {
        self.0.len() / 8
    }
    fn data(&self) -> std::borrow::Cow<'_, [u8]> {
        std::borrow::Cow::Borrowed(&self.0)
    }
}

/// The IFD0 tags an EXIF block may contribute: descriptive ones only, never one the image
/// encoder owns (dimensions, strips, …).
const IFD0_TAGS: [exif::Tag; 6] = [
    exif::Tag::Make,
    exif::Tag::Model,
    exif::Tag::DateTime,
    exif::Tag::Artist,
    exif::Tag::Copyright,
    exif::Tag::Orientation,
];

/// The image's EXIF block split by where a TIFF keeps each part.
#[derive(Default)]
struct Metadata {
    ifd0: Vec<exif::Field>,
    exif: Vec<exif::Field>,
    gps: Vec<exif::Field>,
}

impl Metadata {
    fn split(block: Option<&[u8]>) -> Metadata {
        let mut out = Metadata::default();
        for f in block.map(crate::metadata::fields).unwrap_or_default() {
            match f.tag.context() {
                exif::Context::Tiff if IFD0_TAGS.contains(&f.tag) => out.ifd0.push(f),
                // The sub-IFD pointers are ours to write, never the block's.
                exif::Context::Exif if f.tag != exif::Tag::InteropIFDPointer => out.exif.push(f),
                exif::Context::Gps => out.gps.push(f),
                _ => {}
            }
        }
        out
    }
}

/// Where the sub-IFDs landed, for IFD0's pointers.
#[derive(Default, Clone, Copy)]
struct SubIfds {
    exif: Option<u32>,
    gps: Option<u32>,
}

/// Write one EXIF field into a TIFF directory, as the same type it has in the block.
fn put<W: Write + Seek, K: TiffKind>(
    dir: &mut DirectoryEncoder<'_, W, K>,
    f: &exif::Field,
) -> Result<(), EncodeError> {
    let tag = Tag::from_u16_exhaustive(f.tag.number());
    match &f.value {
        exif::Value::Ascii(parts) => {
            let mut bytes = Vec::new();
            for p in parts {
                bytes.extend_from_slice(p);
                bytes.push(0);
            }
            dir.write_tag(tag, Ascii(bytes))
        }
        exif::Value::Byte(v) => dir.write_tag(tag, v.as_slice()),
        exif::Value::Short(v) => dir.write_tag(tag, v.as_slice()),
        exif::Value::Long(v) => dir.write_tag(tag, v.as_slice()),
        exif::Value::Undefined(v, _) => dir.write_tag(tag, Undefined(v)),
        exif::Value::Rational(v) => dir.write_tag(
            tag,
            Rationals(
                v.iter()
                    .flat_map(|r| [r.num.to_ne_bytes(), r.denom.to_ne_bytes()])
                    .flatten()
                    .collect(),
            ),
        ),
        exif::Value::SRational(v) => dir.write_tag(
            tag,
            SRationals(
                v.iter()
                    .flat_map(|r| [r.num.to_ne_bytes(), r.denom.to_ne_bytes()])
                    .flatten()
                    .collect(),
            ),
        ),
        // Nothing on the allowlist has another type.
        _ => Ok(()),
    }
    .map_err(codec)
}

/// Write the Exif and GPS sub-IFDs (unchained: only IFD0's pointers reach them).
fn write_sub_ifds<W: Write + Seek>(
    enc: &mut tiff::encoder::TiffEncoder<W>,
    meta: &Metadata,
) -> Result<SubIfds, EncodeError> {
    let mut at = |fields: &[exif::Field]| -> Result<Option<u32>, EncodeError> {
        if fields.is_empty() {
            return Ok(None);
        }
        let mut dir = enc.extra_directory().map_err(codec)?;
        for f in fields {
            put(&mut dir, f)?;
        }
        Ok(Some(dir.finish_with_offsets().map_err(codec)?.offset))
    };
    Ok(SubIfds {
        exif: at(&meta.exif)?,
        gps: at(&meta.gps)?,
    })
}

/// The ICC profile, written as `UNDEFINED` as the TIFF/ICC embedding note specifies (the
/// crate's `[u8]` writes `BYTE`).
struct Undefined<'a>(&'a [u8]);

impl TiffValue for Undefined<'_> {
    const BYTE_LEN: u8 = 1;
    const FIELD_TYPE: Type = Type::UNDEFINED;
    fn count(&self) -> usize {
        self.0.len()
    }
    fn data(&self) -> std::borrow::Cow<'_, [u8]> {
        std::borrow::Cow::Borrowed(self.0)
    }
}

fn codec(e: tiff::TiffError) -> EncodeError {
    EncodeError::Codec(format!("tiff: {e}"))
}

/// Encode `img` as a TIFF.
pub fn encode_tiff(img: &ExportImage) -> Result<Vec<u8>, EncodeError> {
    crate::px::check(img)?;
    let icc = match &img.color {
        OutputColor::Srgb => None,
        OutputColor::Icc(p) => Some(p.as_slice()),
    };
    // Gray + alpha is written as RGBA (the crate has no gray-alpha layout), so a gray profile
    // must become the RGB one with the same curve — a GRAY profile can't describe RGB samples.
    let widened_icc;
    let icc = match icc {
        Some(p)
            if img.channels == Channels::GrayAlpha
                && crate::icc::data_color_space(p) == Some(crate::icc::IccSpace::Gray) =>
        {
            widened_icc = crate::icc::rgb_for_gray(p).filter(|rgb| !crate::icc::is_srgb(rgb));
            widened_icc.as_deref()
        }
        other => other,
    };
    let (w, h) = (img.width, img.height);
    let meta = Metadata::split(img.metadata.exif.as_deref());
    let mut out = Cursor::new(Vec::new());
    {
        let mut enc = tiff::encoder::TiffEncoder::new(&mut out)
            .map_err(codec)?
            .with_compression(Compression::Lzw)
            .with_predictor(Predictor::Horizontal);
        // The sub-IFDs go first: the image's encoder holds `enc` until it is finished.
        let subs = write_sub_ifds(&mut enc, &meta)?;
        let tags = Tags {
            icc,
            ifd0: &meta.ifd0,
            subs,
        };
        match (&img.samples, img.channels) {
            (Samples::U8(v), Channels::Gray) => write::<colortype::Gray8>(&mut enc, w, h, v, &tags),
            (Samples::U8(v), Channels::Rgb) => write::<colortype::RGB8>(&mut enc, w, h, v, &tags),
            (Samples::U8(v), Channels::Rgba) => write::<colortype::RGBA8>(&mut enc, w, h, v, &tags),
            // No gray + alpha layout in the crate: widen to RGBA (still lossless).
            (Samples::U8(v), Channels::GrayAlpha) => {
                let rgba: Vec<u8> = v
                    .chunks_exact(2)
                    .flat_map(|p| [p[0], p[0], p[0], p[1]])
                    .collect();
                write::<colortype::RGBA8>(&mut enc, w, h, &rgba, &tags)
            }
            (Samples::U16(v), Channels::Gray) => {
                write::<colortype::Gray16>(&mut enc, w, h, v, &tags)
            }
            (Samples::U16(v), Channels::Rgb) => write::<colortype::RGB16>(&mut enc, w, h, v, &tags),
            (Samples::U16(v), Channels::Rgba) => {
                write::<colortype::RGBA16>(&mut enc, w, h, v, &tags)
            }
            (Samples::U16(v), Channels::GrayAlpha) => {
                let rgba: Vec<u16> = v
                    .chunks_exact(2)
                    .flat_map(|p| [p[0], p[0], p[0], p[1]])
                    .collect();
                write::<colortype::RGBA16>(&mut enc, w, h, &rgba, &tags)
            }
            (Samples::F32(_), _) => Err(EncodeError::Malformed),
        }?;
    }
    Ok(out.into_inner())
}

/// What IFD0 carries besides the pixels.
struct Tags<'a> {
    icc: Option<&'a [u8]>,
    ifd0: &'a [exif::Field],
    subs: SubIfds,
}

fn write<C: ColorType>(
    enc: &mut tiff::encoder::TiffEncoder<&mut Cursor<Vec<u8>>>,
    w: u32,
    h: u32,
    data: &[C::Inner],
    tags: &Tags<'_>,
) -> Result<(), EncodeError>
where
    [C::Inner]: TiffValue,
{
    let icc = tags.icc;
    let mut image = enc.new_image::<C>(w, h).map_err(codec)?;
    for f in tags.ifd0 {
        put(image.encoder(), f)?;
    }
    if let Some(at) = tags.subs.exif {
        image
            .encoder()
            .write_tag(Tag::ExifDirectory, at)
            .map_err(codec)?;
    }
    if let Some(at) = tags.subs.gps {
        image
            .encoder()
            .write_tag(Tag::GpsDirectory, at)
            .map_err(codec)?;
    }
    // RGBA / gray-alpha: the fourth sample is plain (unassociated) alpha. Written as a tag
    // rather than through `extra_samples`, which would count a fifth sample per pixel.
    if C::BITS_PER_SAMPLE.len() == 4 {
        image
            .encoder()
            .write_tag(Tag::ExtraSamples, &[2u16][..])
            .map_err(codec)?;
    }
    if let Some(p) = icc {
        image
            .encoder()
            .write_tag(Tag::IccProfile, Undefined(p))
            .map_err(codec)?;
    }
    image.write_data(data).map_err(codec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiff::decoder::{Decoder, DecodingResult};

    fn image(
        w: u32,
        h: u32,
        channels: Channels,
        samples: Samples,
        color: OutputColor,
    ) -> ExportImage {
        ExportImage {
            width: w,
            height: h,
            channels,
            samples,
            color,
            notes: Vec::new(),
            metadata: Default::default(),
        }
    }

    /// Required test 2 for TIFF: an EXIF reader finds the descriptive tags in IFD0 and the
    /// rest in real Exif and GPS sub-IFDs; the pixels and profile are untouched and the viewer
    /// still reads it.
    #[test]
    fn metadata_round_trips_as_native_tags() {
        use crate::metadata::tests::{dirty_source, has, value};
        use crate::metadata::MetadataChoice;
        let p3 = crate::icc::display_p3_profile();
        let px: Vec<u16> = (0..4 * 3 * 3).map(|i| (i as u16) * 1501).collect();
        let mut img = image(
            4,
            3,
            Channels::Rgb,
            Samples::U16(px.clone()),
            OutputColor::Icc(p3.clone()),
        );
        img.metadata = dirty_source().output(MetadataChoice::ALL);
        let t = encode_tiff(&img).unwrap();

        // The whole file is the EXIF structure: read its primary IFD and sub-IFDs.
        let exif = exif::Reader::new()
            .read_from_container(&mut Cursor::new(&t))
            .expect("readable as EXIF");
        let tags: Vec<(exif::Tag, String)> = exif
            .fields()
            .filter(|f| f.ifd_num == exif::In::PRIMARY)
            .map(|f| (f.tag, f.display_value().to_string()))
            .collect();
        assert_eq!(value(&tags, exif::Tag::Make), Some("\"Canon\""));
        assert_eq!(value(&tags, exif::Tag::Artist), Some("\"Jane Doe\""));
        assert_eq!(
            value(&tags, exif::Tag::Orientation),
            Some("row 0 at top and column 0 at left")
        );
        assert!(has(&tags, exif::Tag::DateTimeOriginal), "the Exif sub-IFD");
        assert!(has(&tags, exif::Tag::LensSpecification));
        assert!(has(&tags, exif::Tag::GPSLatitude), "the GPS sub-IFD");
        assert_eq!(value(&tags, exif::Tag::GPSLatitudeRef), Some("N"));
        for private in [
            exif::Tag::MakerNote,
            exif::Tag::BodySerialNumber,
            exif::Tag::UserComment,
            exif::Tag::ImageDescription,
        ] {
            assert!(!has(&tags, private), "{private} leaked");
        }

        let (ct, data, icc, _) = decode(&t);
        assert_eq!(ct, tiff::ColorType::RGB(16));
        assert!(matches!(data, DecodingResult::U16(v) if v == px));
        assert_eq!(icc.as_deref(), Some(p3.as_slice()));
        assert!(image::load_from_memory_with_format(&t, image::ImageFormat::Tiff).is_ok());
    }

    fn decode(
        bytes: &[u8],
    ) -> (
        tiff::ColorType,
        DecodingResult,
        Option<Vec<u8>>,
        Option<Vec<u16>>,
    ) {
        let mut d = Decoder::new(Cursor::new(bytes)).unwrap();
        let ct = d.colortype().unwrap();
        let icc = d.get_tag_u8_vec(Tag::IccProfile).ok();
        let extra = d.get_tag_u16_vec(Tag::ExtraSamples).ok();
        (ct, d.read_image().unwrap(), icc, extra)
    }

    /// Lossless at both depths: every sample comes back, the profile byte for byte, and
    /// alpha is marked as plain alpha.
    #[test]
    fn round_trips_8_and_16_bit_with_alpha_and_profile() {
        let p3 = crate::icc::display_p3_profile();
        let px8: Vec<u8> = (0..3 * 2 * 4).map(|i| (i * 11) as u8).collect();
        let t = encode_tiff(&image(
            3,
            2,
            Channels::Rgba,
            Samples::U8(px8.clone()),
            OutputColor::Icc(p3.clone()),
        ))
        .unwrap();
        let (ct, data, icc, extra) = decode(&t);
        assert_eq!(ct, tiff::ColorType::RGBA(8));
        assert!(matches!(data, DecodingResult::U8(v) if v == px8));
        assert_eq!(icc.as_deref(), Some(p3.as_slice()));
        assert_eq!(extra, Some(vec![2]), "unassociated alpha");

        let px16: Vec<u16> = (0..3 * 2 * 3).map(|i| (i as u16) * 3001 + 5).collect();
        let t = encode_tiff(&image(
            3,
            2,
            Channels::Rgb,
            Samples::U16(px16.clone()),
            OutputColor::Srgb,
        ))
        .unwrap();
        let (ct, data, icc, extra) = decode(&t);
        assert_eq!(ct, tiff::ColorType::RGB(16));
        assert!(matches!(data, DecodingResult::U16(v) if v == px16));
        assert_eq!(icc, None, "sRGB embeds nothing");
        assert_eq!(extra, None);
    }

    #[test]
    fn gray_stays_gray_and_gray_alpha_widens_losslessly() {
        let t = encode_tiff(&image(
            2,
            1,
            Channels::Gray,
            Samples::U8(vec![3, 200]),
            OutputColor::Srgb,
        ))
        .unwrap();
        let (ct, data, _, _) = decode(&t);
        assert_eq!(ct, tiff::ColorType::Gray(8));
        assert!(matches!(data, DecodingResult::U8(v) if v == [3, 200]));
        let t = encode_tiff(&image(
            2,
            1,
            Channels::GrayAlpha,
            Samples::U16(vec![7, 8, 9, 10]),
            OutputColor::Srgb,
        ))
        .unwrap();
        let (ct, data, _, _) = decode(&t);
        assert_eq!(ct, tiff::ColorType::RGBA(16));
        assert!(matches!(data, DecodingResult::U16(v) if v == [7, 7, 7, 8, 9, 9, 9, 10]));
    }

    /// The display decoder (the `image` crate, as Blaze reads TIFFs) opens it too.
    #[test]
    fn the_viewer_reads_it() {
        let px: Vec<u8> = (0..16 * 16 * 3).map(|i| (i % 251) as u8).collect();
        let t = encode_tiff(&image(
            16,
            16,
            Channels::Rgb,
            Samples::U8(px.clone()),
            OutputColor::Srgb,
        ))
        .unwrap();
        let back = image::load_from_memory_with_format(&t, image::ImageFormat::Tiff)
            .expect("decodes")
            .to_rgb8();
        assert_eq!(back.into_raw(), px);
    }

    /// Review finding: gray + alpha widens to RGBA, so its gray profile becomes the RGB one
    /// with the same curve — never a GRAY profile on RGB samples. Plain gray keeps its own.
    #[test]
    fn gray_alpha_carries_an_rgb_profile_and_gray_keeps_its_own() {
        let gray22 = moxcms::ColorProfile::new_gray_with_gamma(2.2)
            .encode()
            .unwrap();
        let color = OutputColor::Icc(gray22.clone());
        let t = encode_tiff(&image(
            2,
            1,
            Channels::GrayAlpha,
            Samples::U8(vec![40, 255, 200, 9]),
            color.clone(),
        ))
        .unwrap();
        let (ct, _, icc, _) = decode(&t);
        assert_eq!(ct, tiff::ColorType::RGBA(8));
        let icc = icc.expect("a profile");
        assert_eq!(
            crate::icc::data_color_space(&icc),
            Some(crate::icc::IccSpace::Rgb)
        );
        let t = encode_tiff(&image(
            2,
            1,
            Channels::Gray,
            Samples::U8(vec![40, 200]),
            color,
        ))
        .unwrap();
        let (ct, _, icc, _) = decode(&t);
        assert_eq!(ct, tiff::ColorType::Gray(8));
        assert_eq!(icc.as_deref(), Some(gray22.as_slice()));
    }

    #[test]
    fn malformed_buffers_are_refused() {
        let short = image(
            2,
            2,
            Channels::Rgb,
            Samples::U8(vec![0; 5]),
            OutputColor::Srgb,
        );
        assert_eq!(encode_tiff(&short), Err(EncodeError::Malformed));
        let float = image(
            1,
            1,
            Channels::Rgb,
            Samples::F32(vec![0.0; 3]),
            OutputColor::Srgb,
        );
        assert_eq!(encode_tiff(&float), Err(EncodeError::Malformed));
        assert_eq!(TiffEncoder.extension(), "tif");
        assert!(TiffEncoder.lossless());
    }
}
