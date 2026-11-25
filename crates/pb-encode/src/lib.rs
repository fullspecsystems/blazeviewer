//! Blaze Viewer's export pipeline (task #89; plan of record: `.taskmaster/docs/export-plan.md`).
//!
//! - [`types`] — the export pixel/colour types. `pb-decode` depends on this crate **for these
//!   types only** (`default-features = false`) and returns an [`ExportSource`]; nothing here
//!   depends on `pb-decode` or `pb-app-core` (review F4).
//! - [`prepare`] — source → encoder-ready [`ExportImage`]: un-premultiply, HDR → SDR P3,
//!   colour resolution (incl. the ICC sanitizer), rotation, cleared transparency.
//! - [`icc`] — the profile sanitizer behind *Strip metadata* and the profiles we synthesize.
//! - [`encoder`] — the `Encoder` seam + [`Caps`] (rung 2); [`ExportFormat`] picks a codec and
//!   [`Codec::available`] says which this build carries.
//! - Encoders behind features: `png` (rung 0c), `jpeg` (rung 2; Describe's helper moved from
//!   `pb-decode`), and rung 4's `tiff`, `avif` (pure Rust, on by default), `webp` and `jxl`
//!   (C/C++ libraries, turned on by the shells' build scripts), and `heic` (macOS Image I/O).
//! - [`resize`] — linear-light Lanczos downscaling for `prepare` (rung 2).
//!
//! Everything is pure — no I/O, no threads of its own, no platform code — except the HEIC
//! encoder, which hands the pixels to macOS Image I/O. So it is unit-tested here and the export
//! worker in `pb-app-core` stays a thin orchestration layer.

#[cfg(feature = "avif")]
pub mod avif;
pub mod encoder;
#[cfg(all(feature = "heic", target_os = "macos"))]
pub mod heic;
pub mod icc;
#[cfg(feature = "jpeg")]
pub mod jpeg;
#[cfg(feature = "jxl")]
pub mod jxl;
#[cfg(feature = "png")]
pub mod png;
pub mod prepare;
#[cfg(any(
    feature = "tiff",
    feature = "avif",
    feature = "webp",
    feature = "jxl",
    all(feature = "heic", target_os = "macos")
))]
mod px;
pub mod resize;
#[cfg(feature = "tiff")]
pub mod tiff;
pub mod types;
#[cfg(feature = "webp")]
pub mod webp;

pub use encoder::{Caps, Codec, Encoder, ExportFormat, PngEffort};
#[cfg(feature = "jpeg")]
pub use jpeg::{encode_jpeg, encode_jpeg_rgba8, JpegEncoder};
#[cfg(feature = "png")]
pub use png::{encode_png, PngEncoder};
pub use prepare::{prepare, PrepareError, PrepareSpec};
pub use resize::Resize;
pub use types::{
    AlphaMode, Channels, Cicp, ColorDesc, ExportImage, ExportNote, ExportSource, OutputColor,
    Provenance, SampleEncoding, Samples,
};

/// Why an encode failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// The image buffer doesn't match its dimensions, or holds samples the codec can't take.
    Malformed,
    /// The codec itself failed.
    Codec(String),
    /// The codec panicked (caught by [`catch_panics`]).
    Panicked(String),
    /// This build doesn't carry the codec (a setting saved by a build that did).
    Unavailable(&'static str),
}

impl EncodeError {
    #[cfg(feature = "png")]
    fn from_png(e: ::png::EncodingError) -> EncodeError {
        EncodeError::Codec(format!("png: {e}"))
    }
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::Malformed => write!(f, "image buffer doesn't match its size"),
            EncodeError::Codec(why) => write!(f, "{why}"),
            EncodeError::Panicked(why) => write!(f, "encoder crashed: {why}"),
            EncodeError::Unavailable(codec) => write!(f, "this version can't write {codec}"),
        }
    }
}

impl std::error::Error for EncodeError {}

/// Run `f`, turning a panic into `Err(EncodeError::Panicked)` — the encoder twin of
/// `pb-decode`'s private `catch_panics` (review F4: that one is typed to `DecodedImage`).
/// Third-party codecs can panic on edge cases; an export must fail, not crash the app.
/// Requires the release profile's `panic = "unwind"` (workspace Cargo.toml).
pub fn catch_panics<T>(f: impl FnOnce() -> Result<T, EncodeError>) -> Result<T, EncodeError> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            Err(EncodeError::Panicked(msg))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catch_panics_turns_a_panic_into_an_error() {
        let r: Result<(), _> = catch_panics(|| panic!("boom"));
        assert_eq!(r, Err(EncodeError::Panicked("boom".into())));
        assert_eq!(catch_panics(|| Ok(7)), Ok(7));
    }
}
