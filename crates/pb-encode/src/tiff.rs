//! Lossless **TIFF** over the `tiff` crate (export-plan.md rung 4) — for print and for editing
//! in another app. 8- or 16-bit, gray or RGB; alpha as an unassociated extra sample; the colour
//! profile in the ICC tag (an sRGB image embeds none, as JPEG does). LZW with the horizontal
//! predictor: the compression Photoshop and every TIFF reader handle.

use std::io::Cursor;

use tiff::encoder::colortype::{self, ColorType};
use tiff::encoder::{Compression, Predictor, TiffValue};
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
    fn encode(&self, img: &ExportImage) -> Result<Vec<u8>, EncodeError> {
        encode_tiff(img)
    }
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
    let (w, h) = (img.width, img.height);
    let mut out = Cursor::new(Vec::new());
    {
        let mut enc = tiff::encoder::TiffEncoder::new(&mut out)
            .map_err(codec)?
            .with_compression(Compression::Lzw)
            .with_predictor(Predictor::Horizontal);
        match (&img.samples, img.channels) {
            (Samples::U8(v), Channels::Gray) => write::<colortype::Gray8>(&mut enc, w, h, v, icc),
            (Samples::U8(v), Channels::Rgb) => write::<colortype::RGB8>(&mut enc, w, h, v, icc),
            (Samples::U8(v), Channels::Rgba) => write::<colortype::RGBA8>(&mut enc, w, h, v, icc),
            // No gray + alpha layout in the crate: widen to RGBA (still lossless).
            (Samples::U8(v), Channels::GrayAlpha) => {
                let rgba: Vec<u8> = v
                    .chunks_exact(2)
                    .flat_map(|p| [p[0], p[0], p[0], p[1]])
                    .collect();
                write::<colortype::RGBA8>(&mut enc, w, h, &rgba, icc)
            }
            (Samples::U16(v), Channels::Gray) => write::<colortype::Gray16>(&mut enc, w, h, v, icc),
            (Samples::U16(v), Channels::Rgb) => write::<colortype::RGB16>(&mut enc, w, h, v, icc),
            (Samples::U16(v), Channels::Rgba) => write::<colortype::RGBA16>(&mut enc, w, h, v, icc),
            (Samples::U16(v), Channels::GrayAlpha) => {
                let rgba: Vec<u16> = v
                    .chunks_exact(2)
                    .flat_map(|p| [p[0], p[0], p[0], p[1]])
                    .collect();
                write::<colortype::RGBA16>(&mut enc, w, h, &rgba, icc)
            }
            (Samples::F32(_), _) => Err(EncodeError::Malformed),
        }?;
    }
    Ok(out.into_inner())
}

fn write<C: ColorType>(
    enc: &mut tiff::encoder::TiffEncoder<&mut Cursor<Vec<u8>>>,
    w: u32,
    h: u32,
    data: &[C::Inner],
    icc: Option<&[u8]>,
) -> Result<(), EncodeError>
where
    [C::Inner]: TiffValue,
{
    let mut image = enc.new_image::<C>(w, h).map_err(codec)?;
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
        }
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
