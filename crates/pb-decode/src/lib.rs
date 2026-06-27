//! `pb-decode` — the decode abstraction.
//!
//! Two ideas are first-class here because they are the biggest decode-speed
//! levers (see `.taskmaster/docs/architecture.md`):
//!
//! 1. **Decode-to-fit.** Never decode more pixels than the display can show.
//!    A 24MP JPEG shown on a 7680×3840 screen should be decoded at a reduced
//!    scale (libjpeg-turbo DCT scaling, etc.), often cutting decode time several-
//!    fold. Every [`ImageDecoder`] receives the target [`FitBox`].
//!
//! 2. **Preview-first.** Many files embed a small thumbnail / preview (EXIF, HEIC,
//!    RAW). Surfacing that instantly, then refining to the scaled full decode,
//!    is what makes fast scrubbing feel instant.
//!
//! Backends implement [`ImageDecoder`] and are swappable, so we can A/B (e.g.)
//! `turbojpeg` vs `zune-jpeg` purely through this seam.

pub mod orientation;
mod zune;

pub use zune::{decode_image_file, ZuneJpegDecoder};

/// Pixel layout of a decoded buffer. Kept explicit so the uploader can pick the
/// matching GPU texture format with no guesswork.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 8 bits per channel, R,G,B,A order, straight (non-premultiplied) alpha.
    Rgba8,
    /// 16 bits per channel (for HDR / deep-color sources), R,G,B,A order.
    Rgba16,
}

impl PixelFormat {
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Rgba8 => 4,
            PixelFormat::Rgba16 => 8,
        }
    }
}

/// A decoded image in CPU memory, ready to upload to a GPU texture.
#[derive(Debug, Clone)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// Original file dimensions (before decode-to-fit downscaling, after EXIF
    /// orientation) — what the info panel reports as the photo's resolution.
    pub orig_width: u32,
    pub orig_height: u32,
    /// Content-derived codec name (e.g. "JPEG"), not the file extension.
    pub codec: &'static str,
    pub format: PixelFormat,
    /// Tightly packed pixel data, `width * height * format.bytes_per_pixel()`.
    pub pixels: Vec<u8>,
    /// True if this is a fast preview/thumbnail to be refined later.
    pub is_preview: bool,
}

impl DecodedImage {
    pub fn expected_len(width: u32, height: u32, format: PixelFormat) -> usize {
        width as usize * height as usize * format.bytes_per_pixel()
    }

    /// Cheap invariant check used by tests and debug assertions.
    pub fn is_well_formed(&self) -> bool {
        self.pixels.len() == Self::expected_len(self.width, self.height, self.format)
    }
}

/// The maximum size we want decoded — the fit-to-screen box. A decoder should
/// return the largest scale that still covers this without exceeding it where it
/// can (e.g. JPEG 1/2, 1/4, 1/8). `None` means "full resolution".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FitBox {
    pub max_width: u32,
    pub max_height: u32,
}

/// A unit of decode work.
#[derive(Debug, Clone, Copy)]
pub struct DecodeRequest<'a> {
    pub bytes: &'a [u8],
    /// Decode-to-fit target; `None` decodes at full resolution.
    pub fit: Option<FitBox>,
    /// If set, the decoder may return an embedded preview first.
    pub allow_preview: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// This backend does not handle the given bytes.
    Unsupported,
    /// The bytes were recognized but malformed.
    Corrupt(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Unsupported => write!(f, "unsupported format"),
            DecodeError::Corrupt(why) => write!(f, "corrupt image: {why}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// A swappable decode backend. Implementors must be cheap to share across the
/// decode thread pool (`Send + Sync`).
pub trait ImageDecoder: Send + Sync {
    /// Quick sniff: can this backend decode these bytes? Should inspect only the
    /// header, never decode.
    fn can_decode(&self, bytes: &[u8]) -> bool;

    /// Decode to an RGBA buffer, honoring `req.fit` where possible.
    fn decode(&self, req: &DecodeRequest) -> Result<DecodedImage, DecodeError>;

    /// Stable name for logs and A/B benchmark labels (e.g. `"turbojpeg"`).
    fn name(&self) -> &'static str;
}

/// A trivial in-memory decoder used to exercise the trait and as a render
/// placeholder before real codecs are wired in. Produces a solid-color image at
/// the requested fit size (or 1×1 at full res).
#[derive(Debug, Clone)]
pub struct SolidColorDecoder {
    pub rgba: [u8; 4],
}

impl SolidColorDecoder {
    pub fn new(rgba: [u8; 4]) -> Self {
        Self { rgba }
    }
}

impl ImageDecoder for SolidColorDecoder {
    fn can_decode(&self, _bytes: &[u8]) -> bool {
        true
    }

    fn decode(&self, req: &DecodeRequest) -> Result<DecodedImage, DecodeError> {
        let (w, h) = match req.fit {
            Some(f) => (f.max_width.max(1), f.max_height.max(1)),
            None => (1, 1),
        };
        let mut pixels = Vec::with_capacity(DecodedImage::expected_len(w, h, PixelFormat::Rgba8));
        for _ in 0..(w as usize * h as usize) {
            pixels.extend_from_slice(&self.rgba);
        }
        Ok(DecodedImage {
            width: w,
            height: h,
            orig_width: w,
            orig_height: h,
            codec: "solid",
            format: PixelFormat::Rgba8,
            pixels,
            is_preview: false,
        })
    }

    fn name(&self) -> &'static str {
        "solid-color"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_per_pixel_is_correct() {
        assert_eq!(PixelFormat::Rgba8.bytes_per_pixel(), 4);
        assert_eq!(PixelFormat::Rgba16.bytes_per_pixel(), 8);
    }

    #[test]
    fn solid_decoder_honors_fit_box() {
        let d = SolidColorDecoder::new([10, 20, 30, 255]);
        let req = DecodeRequest {
            bytes: &[],
            fit: Some(FitBox {
                max_width: 4,
                max_height: 2,
            }),
            allow_preview: false,
        };
        let img = d.decode(&req).unwrap();
        assert_eq!((img.width, img.height), (4, 2));
        assert!(img.is_well_formed());
        assert_eq!(&img.pixels[0..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn solid_decoder_full_res_is_one_by_one() {
        let d = SolidColorDecoder::new([0, 0, 0, 255]);
        let req = DecodeRequest {
            bytes: &[],
            fit: None,
            allow_preview: false,
        };
        let img = d.decode(&req).unwrap();
        assert_eq!((img.width, img.height), (1, 1));
        assert!(img.is_well_formed());
    }

    #[test]
    fn error_displays() {
        assert_eq!(DecodeError::Unsupported.to_string(), "unsupported format");
    }
}
