//! PNG encoder (rung 0c — `png` 0.18, which `image` already links; review F24).
//!
//! Writes 8- or 16-bit, gray or RGB, with alpha only when some pixel actually uses it
//! (colour-type reduction, review F25). Colour is tagged with an `sRGB` chunk or an `iCCP`
//! profile — never both (the PNG spec forbids it). No metadata chunks: rung 1 exports are
//! Strip-only.

use std::borrow::Cow;

use crate::encoder::{Caps, Encoder};
use crate::types::{Channels, ExportImage, OutputColor, Samples};
use crate::EncodeError;

/// Compression effort. The default is chosen from corpus measurements (rung 1); `High` is
/// the `png` crate's strongest level, still far from `oxipng`'s minutes-long presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PngEffort {
    Fast,
    #[default]
    Balanced,
    High,
}

/// The PNG export format: everything `prepare` produces (alpha, 16-bit, gray).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PngEncoder {
    pub effort: PngEffort,
}

impl Encoder for PngEncoder {
    fn caps(&self) -> Caps {
        Caps::ALL
    }
    fn extension(&self) -> &'static str {
        "png"
    }
    fn encode(&self, img: &ExportImage) -> Result<Vec<u8>, EncodeError> {
        encode_png(img, self.effort)
    }
}

/// Encode `img` as a PNG.
pub fn encode_png(img: &ExportImage, effort: PngEffort) -> Result<Vec<u8>, EncodeError> {
    let n = img.channels.count();
    let expected = (img.width as usize)
        .checked_mul(img.height as usize)
        .and_then(|p| p.checked_mul(n))
        .ok_or(EncodeError::Malformed)?;
    if expected == 0 || img.samples.len() != expected {
        return Err(EncodeError::Malformed);
    }
    let opaque = img.channels.has_alpha() && alpha_is_opaque(&img.samples, n);
    let channels = if opaque {
        img.channels.without_alpha()
    } else {
        img.channels
    };

    let (bit_depth, data): (png::BitDepth, Cow<[u8]>) = match &img.samples {
        Samples::U8(v) => (
            png::BitDepth::Eight,
            if opaque {
                Cow::Owned(drop_alpha(v, n))
            } else {
                Cow::Borrowed(v.as_slice())
            },
        ),
        Samples::U16(v) => {
            let src: Cow<[u16]> = if opaque {
                Cow::Owned(drop_alpha(v, n))
            } else {
                Cow::Borrowed(v.as_slice())
            };
            (
                png::BitDepth::Sixteen,
                Cow::Owned(src.iter().flat_map(|s| s.to_be_bytes()).collect()),
            )
        }
        Samples::F32(_) => return Err(EncodeError::Malformed),
    };

    let mut info = png::Info::with_size(img.width, img.height);
    info.color_type = match channels {
        Channels::Gray => png::ColorType::Grayscale,
        Channels::GrayAlpha => png::ColorType::GrayscaleAlpha,
        Channels::Rgb => png::ColorType::Rgb,
        Channels::Rgba => png::ColorType::Rgba,
    };
    info.bit_depth = bit_depth;
    match &img.color {
        OutputColor::Srgb => info.srgb = Some(png::SrgbRenderingIntent::Perceptual),
        OutputColor::Icc(p) => info.icc_profile = Some(Cow::Borrowed(p.as_slice())),
    }

    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::with_info(&mut out, info).map_err(EncodeError::from_png)?;
        enc.set_compression(match effort {
            PngEffort::Fast => png::Compression::Fast,
            PngEffort::Balanced => png::Compression::Balanced,
            PngEffort::High => png::Compression::High,
        });
        enc.set_filter(png::Filter::Adaptive);
        let mut writer = enc.write_header().map_err(EncodeError::from_png)?;
        writer
            .write_image_data(&data)
            .map_err(EncodeError::from_png)?;
        writer.finish().map_err(EncodeError::from_png)?;
    }
    Ok(out)
}

fn alpha_is_opaque(samples: &Samples, n: usize) -> bool {
    match samples {
        Samples::U8(v) => v.chunks_exact(n).all(|p| p[n - 1] == u8::MAX),
        Samples::U16(v) => v.chunks_exact(n).all(|p| p[n - 1] == u16::MAX),
        Samples::F32(v) => v.chunks_exact(n).all(|p| p[n - 1] >= 1.0),
    }
}

fn drop_alpha<T: Copy>(v: &[T], n: usize) -> Vec<T> {
    let mut out = Vec::with_capacity(v.len() / n * (n - 1));
    for px in v.chunks_exact(n) {
        out.extend_from_slice(&px[..n - 1]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(
        channels: Channels,
        samples: Samples,
        w: u32,
        h: u32,
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

    /// Decode with the `png` crate itself: (info, raw image bytes).
    fn decode(
        bytes: &[u8],
    ) -> (
        png::ColorType,
        png::BitDepth,
        Option<Vec<u8>>,
        bool,
        Vec<u8>,
    ) {
        let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
        dec.set_transformations(png::Transformations::IDENTITY);
        let mut reader = dec.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let frame = reader.next_frame(&mut buf).unwrap();
        buf.truncate(frame.buffer_size());
        let info = reader.info();
        (
            info.color_type,
            info.bit_depth,
            info.icc_profile.as_ref().map(|p| p.to_vec()),
            info.srgb.is_some(),
            buf,
        )
    }

    #[test]
    fn rgb8_round_trips_with_an_srgb_chunk() {
        let px: Vec<u8> = (0..4 * 3 * 3).map(|i| (i * 13) as u8).collect();
        let png = encode_png(
            &image(
                Channels::Rgb,
                Samples::U8(px.clone()),
                4,
                3,
                OutputColor::Srgb,
            ),
            PngEffort::Fast,
        )
        .unwrap();
        let (ct, bd, icc, srgb, data) = decode(&png);
        assert_eq!((ct, bd), (png::ColorType::Rgb, png::BitDepth::Eight));
        assert!(srgb && icc.is_none());
        assert_eq!(data, px);
    }

    #[test]
    fn sixteen_bit_survives_exactly_with_its_profile() {
        let px: Vec<u16> = (0..3 * 2 * 4)
            .map(|i| (i as u16).wrapping_mul(2741))
            .collect();
        let profile = crate::icc::display_p3_profile();
        let png = encode_png(
            &image(
                Channels::Rgba,
                Samples::U16(px.clone()),
                3,
                2,
                OutputColor::Icc(profile.clone()),
            ),
            PngEffort::Balanced,
        )
        .unwrap();
        let (ct, bd, icc, srgb, data) = decode(&png);
        assert_eq!((ct, bd), (png::ColorType::Rgba, png::BitDepth::Sixteen));
        assert_eq!(
            icc.as_deref(),
            Some(profile.as_slice()),
            "iCCP bytes byte-identical"
        );
        assert!(!srgb, "never both sRGB and iCCP");
        let back: Vec<u16> = data
            .chunks_exact(2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(back, px);
    }

    #[test]
    fn opaque_alpha_is_dropped_but_real_alpha_is_kept() {
        let opaque = image(
            Channels::Rgba,
            Samples::U8(vec![1, 2, 3, 255, 4, 5, 6, 255]),
            2,
            1,
            OutputColor::Srgb,
        );
        let (ct, _, _, _, data) = decode(&encode_png(&opaque, PngEffort::Fast).unwrap());
        assert_eq!(ct, png::ColorType::Rgb);
        assert_eq!(data, vec![1, 2, 3, 4, 5, 6]);
        let translucent = image(
            Channels::Rgba,
            Samples::U8(vec![1, 2, 3, 255, 4, 5, 6, 7]),
            2,
            1,
            OutputColor::Srgb,
        );
        let (ct, _, _, _, _) = decode(&encode_png(&translucent, PngEffort::Fast).unwrap());
        assert_eq!(ct, png::ColorType::Rgba);
    }

    #[test]
    fn gray_stays_gray() {
        let img = image(
            Channels::Gray,
            Samples::U16(vec![0, 65535, 1234]),
            3,
            1,
            OutputColor::Srgb,
        );
        let (ct, bd, _, _, _) = decode(&encode_png(&img, PngEffort::High).unwrap());
        assert_eq!(
            (ct, bd),
            (png::ColorType::Grayscale, png::BitDepth::Sixteen)
        );
    }

    #[test]
    fn mismatched_buffers_and_floats_are_refused() {
        let bad = image(
            Channels::Rgb,
            Samples::U8(vec![0; 5]),
            2,
            2,
            OutputColor::Srgb,
        );
        assert_eq!(
            encode_png(&bad, PngEffort::Fast),
            Err(EncodeError::Malformed)
        );
        let float = image(
            Channels::Rgb,
            Samples::F32(vec![0.0; 3]),
            1,
            1,
            OutputColor::Srgb,
        );
        assert_eq!(
            encode_png(&float, PngEffort::Fast),
            Err(EncodeError::Malformed)
        );
    }
}
