//! The export pipeline's pixel and colour types (export-plan.md § 3.2).
//!
//! An [`ExportSource`] is what `pb_decode::decode_for_export` returns: full resolution, the
//! samples' **true** encoding and colour description (export-plan.md § 3.8 Rule 1), never a
//! display reduction. [`crate::prepare`] turns one into an [`ExportImage`] — rotated,
//! colour-resolved and quantized for a target — which an encoder writes.
//!
//! Identifiers use the American spelling (`Color`) per the house rule for new identifiers;
//! prose stays Canadian.

use std::sync::Arc;

/// Channel layout of a pixel buffer. Samples are interleaved in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channels {
    Gray,
    GrayAlpha,
    Rgb,
    Rgba,
}

impl Channels {
    /// Samples per pixel.
    pub fn count(self) -> usize {
        match self {
            Channels::Gray => 1,
            Channels::GrayAlpha => 2,
            Channels::Rgb => 3,
            Channels::Rgba => 4,
        }
    }

    /// Whether the last channel is alpha.
    pub fn has_alpha(self) -> bool {
        matches!(self, Channels::GrayAlpha | Channels::Rgba)
    }

    /// Whether the colour channels are a single gray channel.
    pub fn is_gray(self) -> bool {
        matches!(self, Channels::Gray | Channels::GrayAlpha)
    }

    /// The same layout without its alpha channel.
    pub fn without_alpha(self) -> Channels {
        match self {
            Channels::GrayAlpha => Channels::Gray,
            Channels::Rgba => Channels::Rgb,
            other => other,
        }
    }
}

/// Interleaved samples. `U16` holds native-endian values (encoders byte-swap as their
/// format requires); `F32` is either source-encoded or linear per [`SampleEncoding`].
#[derive(Debug, Clone, PartialEq)]
pub enum Samples {
    U8(Vec<u8>),
    U16(Vec<u16>),
    F32(Vec<f32>),
}

impl Samples {
    /// Number of samples (not pixels).
    pub fn len(&self) -> usize {
        match self {
            Samples::U8(v) => v.len(),
            Samples::U16(v) => v.len(),
            Samples::F32(v) => v.len(),
        }
    }

    /// Whether there are no samples.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bits per sample.
    pub fn bit_depth(&self) -> u8 {
        match self {
            Samples::U8(_) => 8,
            Samples::U16(_) => 16,
            Samples::F32(_) => 32,
        }
    }
}

/// How the sample values relate to light.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleEncoding {
    /// Encoded by the colour description's transfer curve (the usual case).
    Encoded,
    /// Linear light. For HDR this is the app's named convention: BT.709 primaries,
    /// **1.0 = SDR reference white = 203 nits** (export-plan.md § 3.8 Rule 2; task #146
    /// verifies every backend actually normalizes into it).
    Linear,
}

/// Alpha convention of a buffer that has an alpha channel (ignored otherwise).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlphaMode {
    Straight,
    /// Colour already multiplied by alpha — Quartz bitmap contexts produce this.
    Premultiplied,
}

/// H.273 / ISOBMFF `nclx` code points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cicp {
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub full_range: bool,
}

/// The source's colour description, as faithfully as the backend can give it.
#[derive(Debug, Clone, PartialEq)]
pub enum ColorDesc {
    /// No profile and no code points: treat as sRGB (and tag it so on output).
    Untagged,
    /// The source's embedded ICC profile bytes, verbatim.
    Icc(Arc<[u8]>),
    /// Code points only (HEIF/AVIF `nclx`); a profile is synthesized on output.
    Cicp(Cicp),
    /// Linear HDR in the app convention (pair with [`SampleEncoding::Linear`]).
    LinearScRgb,
}

/// What a decoded source is and where it came from — feeds the details panel and the
/// export's user-facing notes. Never persisted (privacy #2).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Provenance {
    /// Content-derived codec label (`"JPEG"`, `"HEIF"`, …).
    pub codec: &'static str,
    /// The decode path that produced the pixels, for diagnostics (`"zune"`, …).
    pub path: &'static str,
    /// The file was malformed and recovered by a fallback rung (task #127).
    pub recovered: bool,
    /// The pixels come from a camera RAW's embedded preview, not a demosaic.
    pub raw_preview: bool,
}

/// A full-resolution decode for export, honestly described.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportSource {
    pub width: u32,
    pub height: u32,
    pub channels: Channels,
    pub samples: Samples,
    pub encoding: SampleEncoding,
    pub alpha: AlphaMode,
    pub color: ColorDesc,
    pub provenance: Provenance,
}

impl ExportSource {
    /// Whether `samples` holds exactly `width × height × channels` values.
    pub fn is_well_formed(&self) -> bool {
        (self.width as usize)
            .checked_mul(self.height as usize)
            .and_then(|px| px.checked_mul(self.channels.count()))
            .is_some_and(|n| n == self.samples.len() && n > 0)
    }

    /// Whether this is HDR content (linear light that may exceed SDR white).
    pub fn is_hdr(&self) -> bool {
        self.encoding == SampleEncoding::Linear
    }
}

/// The colour tag an encoder writes.
#[derive(Debug, Clone, PartialEq)]
pub enum OutputColor {
    /// sRGB — PNG writes an `sRGB` chunk; formats without one embed nothing (sRGB implied).
    Srgb,
    /// An ICC profile to embed (sanitized unless the user kept the original).
    Icc(Vec<u8>),
}

/// Something the user should know about how the export differs from the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportNote {
    /// HDR content was tone-mapped to SDR (Display P3) for a format/rung without HDR.
    HdrToneMapped,
    /// The source profile was rebuilt without its descriptive text (Strip metadata).
    ProfileSanitized,
    /// The source profile couldn't be used (unreadable, or didn't match the pixels), so
    /// the export is tagged sRGB.
    ProfileReplacedWithSrgb,
    /// Fully transparent pixels had their hidden colour cleared (Strip metadata).
    TransparentColorCleared,
}

/// An image ready to encode: rotated, colour-resolved, integer samples.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportImage {
    pub width: u32,
    pub height: u32,
    pub channels: Channels,
    /// `U8` or `U16` only — [`crate::prepare`] never emits `F32`.
    pub samples: Samples,
    pub color: OutputColor,
    pub notes: Vec<ExportNote>,
}
