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

mod color;
mod common;
mod image_backend;
mod jxl;
pub mod metadata;
pub mod orientation;
mod raw;
mod svg;
#[cfg(windows)]
mod wic;
mod zune;

use std::path::Path;

pub use color::ColorTransform;
pub use image_backend::ImageCrateDecoder;
pub use jxl::JxlDecoder;
pub use metadata::read_exif_fields;
pub use raw::RawPreviewDecoder;
pub use svg::SvgDecoder;
#[cfg(windows)]
pub use wic::WicDecoder;
pub use zune::ZuneJpegDecoder;

/// Pixel layout of a decoded buffer. Kept explicit so the uploader can pick the
/// matching GPU texture format with no guesswork.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 8 bits per channel, R,G,B,A order, straight (non-premultiplied) alpha.
    /// sRGB-encoded (source-encoded); the renderer linearizes per `ColorTransform`.
    Rgba8,
    /// 16-bit **half-float** per channel, R,G,B,A. Carries HDR / wide-gamut sources
    /// as **scene-linear scRGB** (linear, BT.709 primaries, extended range), ready
    /// to present to an fp16 scRGB swapchain.
    Rgba16F,
}

impl PixelFormat {
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Rgba8 => 4,
            PixelFormat::Rgba16F => 8,
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
    /// Source→sRGB color conversion for the renderer (in-shader matrix + TRC).
    /// Defaults to sRGB passthrough; backends override it when the source carries
    /// a wide-gamut ICC profile (Display-P3 HEIC, Adobe RGB JPEG, …).
    pub color: ColorTransform,
    /// Peak scene-linear value, for HDR (`Rgba16F`) images — the tone-map white
    /// point used when presenting to an SDR display. 1.0 for SDR sources.
    pub peak: f32,
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
            color: ColorTransform::srgb(),
            peak: 1.0,
        })
    }

    fn name(&self) -> &'static str {
        "solid-color"
    }
}

/// Decode in-memory image `bytes` by sniffing the format and routing to the
/// first backend that recognizes it. Backends are tried specific-first (JPEG,
/// then the broad `image` crate); RAW and SVG are *not* sniffable by magic alone
/// (RAW is TIFF-shaped, SVG is text), so [`decode_image_file`] routes those by
/// extension before falling through to here.
pub fn decode_bytes(
    bytes: &[u8],
    fit: Option<FitBox>,
    allow_preview: bool,
) -> Result<DecodedImage, DecodeError> {
    catch_panics(|| decode_bytes_inner(bytes, fit, allow_preview))
}

fn decode_bytes_inner(
    bytes: &[u8],
    fit: Option<FitBox>,
    allow_preview: bool,
) -> Result<DecodedImage, DecodeError> {
    let req = DecodeRequest {
        bytes,
        fit,
        allow_preview,
    };
    // Specific magic-sniffable backends first, the broad `image` crate last.
    let jpeg = ZuneJpegDecoder;
    let jxl = JxlDecoder;
    let images = ImageCrateDecoder;
    let mut backends: Vec<&dyn ImageDecoder> = vec![&jpeg, &jxl];
    // AVIF + HEIC via the OS imaging codecs on Windows (pure-Rust elsewhere can't
    // do AV1/HEVC). Tried before the `image` crate, which doesn't handle them here.
    #[cfg(windows)]
    let wic = WicDecoder;
    #[cfg(windows)]
    backends.push(&wic);
    backends.push(&images);
    for backend in backends {
        if backend.can_decode(bytes) {
            return backend.decode(&req);
        }
    }
    Err(DecodeError::Unsupported)
}

/// Read a file from disk and decode it to an upright RGBA image, downscaled to
/// fit `fit` (the display size) when provided. The single entry point the app's
/// decode pool calls; it picks the backend (by extension for the ambiguous RAW/
/// SVG cases, otherwise by content sniff via [`decode_bytes`]).
///
/// **Panic-safe:** a decoder that panics on a malformed/hostile file is caught and
/// reported as a [`DecodeError`] rather than unwinding into the decode pool — the
/// viewer skips the bad file instead of dying. (A hard stack overflow is still
/// fatal and is mitigated separately; see the RAW demosaic's big-stack thread.)
pub fn decode_image_file(path: &Path, fit: Option<FitBox>) -> Result<DecodedImage, DecodeError> {
    catch_panics(|| decode_image_file_inner(path, fit))
}

fn decode_image_file_inner(path: &Path, fit: Option<FitBox>) -> Result<DecodedImage, DecodeError> {
    let bytes =
        std::fs::read(path).map_err(|e| DecodeError::Corrupt(format!("read error: {e}")))?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let req = DecodeRequest {
        bytes: &bytes,
        fit,
        allow_preview: true,
    };
    // RAW (TIFF-shaped) and SVG (XML text) can't be told apart from a plain TIFF
    // or arbitrary text by a magic sniff, so route them by extension; everything
    // else sniffs by content.
    if raw::is_raw_extension(&ext) {
        return RawPreviewDecoder.decode(&req);
    }
    if matches!(ext.as_str(), "svg" | "svgz") {
        return SvgDecoder.decode(&req);
    }
    // TGA is headerless (no magic number), so content-sniffing can't find it —
    // route by extension with an explicit format hint.
    if ext == "tga" {
        return image_backend::decode_tga(&bytes, fit);
    }
    decode_bytes_inner(&bytes, fit, false)
}

/// Run a decode, converting a panic into a `DecodeError::Corrupt` instead of
/// letting it unwind. Decoders here parse hostile, third-party bytes and some
/// (jxl-oxide, resvg, the RAW pipeline) can panic on malformed input; a viewer
/// pointed at a large library must never die on one bad file. Requires the
/// release profile to use `panic = "unwind"` (see the workspace Cargo.toml).
fn catch_panics<F>(f: F) -> Result<DecodedImage, DecodeError>
where
    F: FnOnce() -> Result<DecodedImage, DecodeError>,
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => Err(DecodeError::Corrupt(format!(
            "decoder panicked: {}",
            panic_message(&*payload)
        ))),
    }
}

/// Best-effort text from a panic payload (`&str` / `String`, else a placeholder).
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// Whether a file extension (without the dot, any case) is one we attempt to
/// decode — the single source of truth the directory scanner filters on, kept in
/// sync with the backends above. AVIF/HEIC are advertised only on Windows (where
/// the WIC backend can reach the OS codecs), so the scanner advertises only what
/// the build can actually decode.
pub fn is_supported_extension(ext: &str) -> bool {
    let e = ext.to_ascii_lowercase();
    const BASE: &[&str] = &[
        // JPEG (zune)
        "jpg", "jpeg", "jpe", "jfif", //
        // image crate (pure Rust)
        "png", "gif", "bmp", "dib", "ico", "tif", "tiff", "webp", "tga", "qoi", "pnm", "pbm", "pgm",
        "ppm", "pam", "hdr", "exr", "ff", "dds", //
        // JXL, SVG
        "jxl", "svg", "svgz", //
        // RAW (embedded-preview path)
        "arw", "nef", "cr2", "cr3", "dng", "raf", "rw2", "orf", "srw", "pef", "raw",
    ];
    if BASE.contains(&e.as_str()) {
        return true;
    }
    // AVIF/HEIC are decoded via WIC on Windows (OS codec extensions).
    #[cfg(windows)]
    if matches!(e.as_str(), "avif" | "heic" | "heif" | "hif") {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_extension_is_case_insensitive_and_covers_formats() {
        assert!(is_supported_extension("jpg"));
        assert!(is_supported_extension("JPG"));
        assert!(is_supported_extension("PNG"));
        assert!(is_supported_extension("qoi"));
        assert!(is_supported_extension("webp"));
        assert!(is_supported_extension("jxl"));
        assert!(is_supported_extension("svg"));
        assert!(is_supported_extension("nef"));
        assert!(is_supported_extension("arw"));
        assert!(!is_supported_extension("txt"));
        assert!(!is_supported_extension("mp4"));
    }

    /// Encode a 3x2 solid image to `fmt` in memory (round-trip fodder for the
    /// dispatch tests).
    fn encode(fmt: image::ImageFormat) -> Vec<u8> {
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            3,
            2,
            image::Rgba([10, 20, 30, 255]),
        ));
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, fmt).expect("encode");
        buf.into_inner()
    }

    #[test]
    fn dispatch_routes_formats_to_the_right_backend() {
        // Magic-sniffable formats route purely on content via decode_bytes.
        for (fmt, codec) in [
            (image::ImageFormat::Png, "PNG"),
            (image::ImageFormat::Qoi, "QOI"),
            (image::ImageFormat::Bmp, "BMP"),
        ] {
            let bytes = encode(fmt);
            let img = decode_bytes(&bytes, None, false).unwrap_or_else(|e| panic!("{codec}: {e}"));
            assert_eq!(img.codec, codec, "{codec} routed to wrong backend");
            assert_eq!((img.orig_width, img.orig_height), (3, 2), "{codec} dims");
            assert!(img.is_well_formed(), "{codec} buffer");
        }
    }

    #[test]
    fn tga_is_routed_by_extension() {
        // TGA has no magic number, so it can't be content-sniffed — it must be
        // routed by the .tga extension through decode_image_file.
        let bytes = encode(image::ImageFormat::Tga);
        let path = std::env::temp_dir().join(format!("pb_tga_test_{}.tga", std::process::id()));
        std::fs::write(&path, &bytes).expect("write temp tga");
        let decoded = decode_image_file(&path, None);
        let _ = std::fs::remove_file(&path);
        let img = decoded.expect("tga decode");
        assert_eq!(img.codec, "TGA");
        assert_eq!((img.orig_width, img.orig_height), (3, 2));
        assert!(img.is_well_formed());
    }

    #[test]
    fn svg_decoder_rasterizes_to_image() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="8"><rect width="10" height="8" fill="red"/></svg>"#;
        let req = DecodeRequest {
            bytes: svg,
            fit: None,
            allow_preview: false,
        };
        let img = SvgDecoder.decode(&req).expect("svg decode");
        assert_eq!(img.codec, "SVG");
        assert_eq!((img.orig_width, img.orig_height), (10, 8));
        assert!(img.is_well_formed());
    }

    #[test]
    fn garbage_bytes_are_an_error_not_a_panic() {
        // Unrecognized bytes must come back as an error, never a panic.
        let r = decode_bytes(&[0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11], None, false);
        assert!(r.is_err());
    }

    #[test]
    fn catch_panics_converts_a_decoder_panic_to_an_error() {
        // Silence the default panic print so the test output stays clean.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let r = catch_panics(|| panic!("boom inside a decoder"));
        std::panic::set_hook(prev);
        match r {
            Err(DecodeError::Corrupt(m)) => {
                assert!(m.contains("panicked"), "msg: {m}");
                assert!(m.contains("boom"), "msg: {m}");
            }
            other => panic!("expected a caught-panic error, got {other:?}"),
        }
    }

    #[test]
    fn bytes_per_pixel_is_correct() {
        assert_eq!(PixelFormat::Rgba8.bytes_per_pixel(), 4);
        assert_eq!(PixelFormat::Rgba16F.bytes_per_pixel(), 8);
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
