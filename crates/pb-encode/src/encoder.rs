//! The **`Encoder` seam** (export-plan.md rung 2): one trait every output format implements,
//! plus [`Caps`] — what the format can carry, which tells [`prepare`](crate::prepare) what to
//! reduce (flatten alpha, narrow 16-bit to 8). Rung 4's codecs (TIFF, WebP, AVIF, JPEG XL,
//! HEIC) slot in behind the same trait; the export worker only ever sees `dyn Encoder`.
//!
//! [`ExportFormat`] names every format whether or not this build carries its codec, so the
//! app's settings and panel never need a `cfg`: [`Codec::available`] says what was built in,
//! and [`ExportFormat::encoder`] refuses the rest with [`EncodeError::Unavailable`].

use crate::types::ExportImage;
use crate::EncodeError;

/// What an output format can carry. `prepare` reduces the image to fit, so an encoder is only
/// ever handed pixels it can write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    /// An alpha channel. Without it, `prepare` flattens onto the spec's background.
    pub alpha: bool,
    /// 16-bit samples. Without it, `prepare` narrows to 8-bit.
    pub sixteen_bit: bool,
    /// A gray layout. Without it, `prepare` widens gray to RGB — and a gray profile to an RGB
    /// one with the same curve, which renders the neutral pixels identically.
    pub gray: bool,
}

impl Caps {
    /// Everything `prepare` can produce (PNG, TIFF, JPEG XL).
    pub const ALL: Caps = Caps {
        alpha: true,
        sixteen_bit: true,
        gray: true,
    };
    /// 8-bit colour with alpha, no gray layout (WebP, AVIF, HEIC).
    pub const EIGHT_BIT_ALPHA: Caps = Caps {
        alpha: true,
        sixteen_bit: false,
        gray: false,
    };
}

/// An output format, configured.
pub trait Encoder: Send + Sync {
    /// What this format can carry — pass it to `prepare` as [`PrepareSpec::caps`](crate::PrepareSpec).
    fn caps(&self) -> Caps;
    /// The file extension, without the dot (`"png"`, `"jpg"`).
    fn extension(&self) -> &'static str;
    /// Whether the file decodes to exactly the pixels it was given, so a preview can show
    /// the prepared image instead of reading the file back.
    fn lossless(&self) -> bool;
    /// Whether this format embeds the image's EXIF block (`ExportImage::metadata.exif`). The
    /// export panel says which formats can't carry metadata. Every codec answers explicitly,
    /// so a new one can't silently drop what the user chose to keep.
    fn writes_metadata(&self) -> bool;
    /// Whether this format writes generation text (`ExportImage::metadata.text`). Only PNG.
    fn writes_generation(&self) -> bool {
        false
    }
    /// Encode an image `prepare`d with [`caps`](Self::caps). Wrapped in
    /// [`catch_panics`](crate::catch_panics) by the caller.
    fn encode(&self, img: &ExportImage) -> Result<Vec<u8>, EncodeError>;
}

/// PNG compression effort. The default is chosen from corpus measurements (rung 1); `High` is
/// the `png` crate's strongest level, still far from `oxipng`'s minutes-long presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PngEffort {
    Fast,
    #[default]
    Balanced,
    High,
}

/// A codec, unconfigured: what [`ExportFormat`] picks, and what [`available`](Self::available)
/// answers for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Png,
    Jpeg,
    Webp,
    Avif,
    JpegXl,
    Heic,
    Tiff,
}

impl Codec {
    /// Whether this build carries the codec. HEIC is macOS only (Image I/O; there is no free
    /// HEVC encoder elsewhere). WebP and JPEG XL are C/C++ libraries behind features the
    /// shells' build scripts turn on.
    pub const fn available(self) -> bool {
        match self {
            Codec::Png => cfg!(feature = "png"),
            Codec::Jpeg => cfg!(feature = "jpeg"),
            Codec::Webp => cfg!(feature = "webp"),
            Codec::Avif => cfg!(feature = "avif"),
            Codec::JpegXl => cfg!(feature = "jxl"),
            Codec::Heic => cfg!(all(feature = "heic", target_os = "macos")),
            Codec::Tiff => cfg!(feature = "tiff"),
        }
    }

    /// Whether the format embeds EXIF: the same answer as its encoder's
    /// [`Encoder::writes_metadata`], without building one (for the panel's notes).
    pub const fn writes_metadata(self) -> bool {
        match self {
            Codec::Png
            | Codec::Jpeg
            | Codec::Webp
            | Codec::Avif
            | Codec::JpegXl
            | Codec::Heic
            | Codec::Tiff => true,
        }
    }

    /// Whether the format writes generation text ([`Encoder::writes_generation`]): PNG only.
    pub const fn writes_generation(self) -> bool {
        matches!(self, Codec::Png)
    }

    /// The name for messages ("this build can't write WebP").
    pub fn name(self) -> &'static str {
        match self {
            Codec::Png => "PNG",
            Codec::Jpeg => "JPEG",
            Codec::Webp => "WebP",
            Codec::Avif => "AVIF",
            Codec::JpegXl => "JPEG XL",
            Codec::Heic => "HEIC",
            Codec::Tiff => "TIFF",
        }
    }
}

/// The formats, with their settings — the value the export panel edits and the export
/// request freezes (plain data; [`encoder`](Self::encoder) makes the codec). Qualities are
/// 1–100; each codec maps it onto its own scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Png(PngEffort),
    /// Baseline JPEG.
    Jpeg {
        quality: u8,
    },
    /// Lossy WebP at `quality`, or lossless (which ignores it).
    Webp {
        quality: u8,
        lossless: bool,
    },
    Avif {
        quality: u8,
    },
    /// Lossy JPEG XL at `quality` (libjxl's quality → distance mapping), or lossless.
    JpegXl {
        quality: u8,
        lossless: bool,
    },
    Heic {
        quality: u8,
    },
    /// Lossless, LZW-compressed TIFF.
    Tiff,
}

impl ExportFormat {
    pub fn codec(self) -> Codec {
        match self {
            ExportFormat::Png(_) => Codec::Png,
            ExportFormat::Jpeg { .. } => Codec::Jpeg,
            ExportFormat::Webp { .. } => Codec::Webp,
            ExportFormat::Avif { .. } => Codec::Avif,
            ExportFormat::JpegXl { .. } => Codec::JpegXl,
            ExportFormat::Heic { .. } => Codec::Heic,
            ExportFormat::Tiff => Codec::Tiff,
        }
    }

    /// The configured codec, or [`EncodeError::Unavailable`] when this build doesn't carry it
    /// (a setting saved by a build that did).
    pub fn encoder(self) -> Result<Box<dyn Encoder>, EncodeError> {
        let codec = self.codec();
        let unavailable = || EncodeError::Unavailable(codec.name());
        #[allow(unused_variables)] // the arms for codecs this build lacks are cfg'd out
        match self {
            #[cfg(feature = "png")]
            ExportFormat::Png(effort) => Ok(Box::new(crate::png::PngEncoder { effort })),
            #[cfg(feature = "jpeg")]
            ExportFormat::Jpeg { quality } => Ok(Box::new(crate::jpeg::JpegEncoder { quality })),
            #[cfg(feature = "webp")]
            ExportFormat::Webp { quality, lossless } => {
                Ok(Box::new(crate::webp::WebpEncoder { quality, lossless }))
            }
            #[cfg(feature = "avif")]
            ExportFormat::Avif { quality } => Ok(Box::new(crate::avif::AvifEncoder { quality })),
            #[cfg(feature = "jxl")]
            ExportFormat::JpegXl { quality, lossless } => {
                Ok(Box::new(crate::jxl::JxlEncoder { quality, lossless }))
            }
            #[cfg(all(feature = "heic", target_os = "macos"))]
            ExportFormat::Heic { quality } => Ok(Box::new(crate::heic::HeicEncoder { quality })),
            #[cfg(feature = "tiff")]
            ExportFormat::Tiff => Ok(Box::new(crate::tiff::TiffEncoder)),
            #[allow(unreachable_patterns)]
            _ => Err(unavailable()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every format this build says it carries makes an encoder, with a dotless extension;
    /// the rest refuse by name instead of panicking.
    #[test]
    fn available_codecs_make_encoders_and_the_rest_refuse() {
        let all = [
            ExportFormat::Png(PngEffort::Balanced),
            ExportFormat::Jpeg { quality: 85 },
            ExportFormat::Webp {
                quality: 80,
                lossless: false,
            },
            ExportFormat::Avif { quality: 70 },
            ExportFormat::JpegXl {
                quality: 90,
                lossless: false,
            },
            ExportFormat::Heic { quality: 80 },
            ExportFormat::Tiff,
        ];
        for f in all {
            match f.encoder() {
                Ok(e) => {
                    assert!(f.codec().available(), "{f:?}");
                    assert!(!e.extension().is_empty() && !e.extension().starts_with('.'));
                    assert_eq!(e.writes_metadata(), f.codec().writes_metadata(), "{f:?}");
                    assert_eq!(
                        e.writes_generation(),
                        f.codec().writes_generation(),
                        "{f:?}"
                    );
                }
                Err(EncodeError::Unavailable(name)) => {
                    assert!(!f.codec().available(), "{f:?}");
                    assert_eq!(name, f.codec().name());
                }
                Err(e) => panic!("{f:?}: {e}"),
            }
        }
    }
}
