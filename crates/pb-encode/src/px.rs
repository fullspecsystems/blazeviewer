//! Small pixel-buffer helpers the rung 4 encoders share.

use crate::types::{Channels, ExportImage, Samples};
use crate::EncodeError;

/// Check that `img` holds exactly `width × height × channels` samples and is non-empty.
pub(crate) fn check(img: &ExportImage) -> Result<(), EncodeError> {
    let expected = (img.width as usize)
        .checked_mul(img.height as usize)
        .and_then(|n| n.checked_mul(img.channels.count()))
        .ok_or(EncodeError::Malformed)?;
    if expected == 0 || img.samples.len() != expected {
        return Err(EncodeError::Malformed);
    }
    Ok(())
}

/// The 8-bit samples of a prepared image, widened to RGB or RGBA — for codecs with no gray
/// layout (WebP, AVIF, HEIC). Returns the samples and whether they carry alpha. 16-bit and
/// float samples are refused: those codecs' `Caps` exclude them, so `prepare` never makes them.
pub(crate) fn rgb8(img: &ExportImage) -> Result<(std::borrow::Cow<'_, [u8]>, bool), EncodeError> {
    use std::borrow::Cow;
    check(img)?;
    let Samples::U8(px) = &img.samples else {
        return Err(EncodeError::Malformed);
    };
    Ok(match img.channels {
        Channels::Rgb => (Cow::Borrowed(px.as_slice()), false),
        Channels::Rgba => (Cow::Borrowed(px.as_slice()), true),
        Channels::Gray => (
            Cow::Owned(px.iter().flat_map(|&g| [g, g, g]).collect()),
            false,
        ),
        Channels::GrayAlpha => (
            Cow::Owned(
                px.chunks_exact(2)
                    .flat_map(|p| [p[0], p[0], p[0], p[1]])
                    .collect(),
            ),
            true,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::OutputColor;

    fn img(channels: Channels, samples: Samples) -> ExportImage {
        ExportImage {
            width: 2,
            height: 1,
            channels,
            samples,
            color: OutputColor::Srgb,
            notes: Vec::new(),
        }
    }

    #[test]
    fn gray_widens_and_rgb_passes_through() {
        let gray = img(Channels::Gray, Samples::U8(vec![1, 2]));
        let (px, a) = rgb8(&gray).unwrap();
        assert_eq!((&*px, a), (&[1, 1, 1, 2, 2, 2][..], false));
        let gray_alpha = img(Channels::GrayAlpha, Samples::U8(vec![1, 9, 2, 8]));
        let (px, a) = rgb8(&gray_alpha).unwrap();
        assert_eq!((&*px, a), (&[1, 1, 1, 9, 2, 2, 2, 8][..], true));
        let rgba = img(Channels::Rgba, Samples::U8(vec![1; 8]));
        let (px, a) = rgb8(&rgba).unwrap();
        assert!(matches!(px, std::borrow::Cow::Borrowed(_)) && a);
    }

    #[test]
    fn wrong_sizes_and_deep_samples_are_refused() {
        assert_eq!(
            rgb8(&img(Channels::Rgb, Samples::U8(vec![0; 5]))).err(),
            Some(EncodeError::Malformed)
        );
        assert_eq!(
            rgb8(&img(Channels::Rgb, Samples::U16(vec![0; 6]))).err(),
            Some(EncodeError::Malformed)
        );
    }
}
