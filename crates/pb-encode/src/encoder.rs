//! The **`Encoder` seam** (export-plan.md rung 2): one trait every output format implements,
//! plus [`Caps`] — what the format can carry, which tells [`prepare`](crate::prepare) what to
//! reduce (flatten alpha, narrow 16-bit to 8). Rung 4's codecs (WebP, AVIF, JPEG XL, mozjpeg)
//! slot in behind the same trait; the export worker only ever sees `dyn Encoder`.

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
}

impl Caps {
    /// Everything `prepare` can produce (PNG).
    pub const ALL: Caps = Caps {
        alpha: true,
        sixteen_bit: true,
    };
}

/// An output format, configured.
pub trait Encoder: Send + Sync {
    /// What this format can carry — pass it to `prepare` as [`PrepareSpec::caps`](crate::PrepareSpec).
    fn caps(&self) -> Caps;
    /// The file extension, without the dot (`"png"`, `"jpg"`).
    fn extension(&self) -> &'static str;
    /// Encode an image `prepare`d with [`caps`](Self::caps). Wrapped in
    /// [`catch_panics`](crate::catch_panics) by the caller.
    fn encode(&self, img: &ExportImage) -> Result<Vec<u8>, EncodeError>;
}

/// The formats this build can write, with their settings — the value the export panel edits
/// and the export request freezes (plain data; [`encoder`](Self::encoder) makes the codec).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    #[cfg(feature = "png")]
    Png(crate::png::PngEffort),
    /// Baseline JPEG; `quality` 1–100.
    #[cfg(feature = "jpeg")]
    Jpeg { quality: u8 },
}

impl ExportFormat {
    /// The configured codec.
    pub fn encoder(self) -> Box<dyn Encoder> {
        match self {
            #[cfg(feature = "png")]
            ExportFormat::Png(effort) => Box::new(crate::png::PngEncoder { effort }),
            #[cfg(feature = "jpeg")]
            ExportFormat::Jpeg { quality } => Box::new(crate::jpeg::JpegEncoder { quality }),
        }
    }
}
