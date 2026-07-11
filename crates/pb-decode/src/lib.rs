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

pub mod animation;
mod color;
mod common;
mod image_backend;
#[cfg(target_os = "macos")]
mod imageio;
// Apple Live Photo motion (.mov) decode via AVFoundation's AVAssetReader — macOS only,
// streaming (tasks #38 / #69: frames emit as they decode, so playback starts immediately).
#[cfg(target_os = "macos")]
mod livephoto;
// The Windows mirror: the same .mov decode via Media Foundation, streaming (tasks #39 / #69:
// frames emit as they decode, so playback starts immediately).
#[cfg(windows)]
mod mf_video;
// The Linux mirror: .mov motion + audio decode via FFmpeg, behind the `livephoto` feature.
#[cfg(all(unix, not(target_os = "macos"), feature = "livephoto"))]
mod ff_live;
// The dav1d AV1 backend for animated AVIF (avis) playback — Windows-only, task #76.
// `av1_dav1d` is set by build.rs when the `dav1d` feature is on for a Windows target
// (macOS plays avis via Image I/O, Linux via FFmpeg; elsewhere the feature is a no-op).
#[cfg(av1_dav1d)]
mod dav1d;
// The avis (animated AVIF) demuxer + probe + dav1d decode pipeline (task #76).
// The demux half is pure Rust and deliberately self-contained, so it compiles
// under `test` on every platform (demux unit tests need no decoder) and under
// the hidden `fuzz-internals` feature for the fuzz harness; only the decode
// section inside is gated on the linked dav1d.
#[cfg(any(av1_dav1d, test, feature = "fuzz-internals"))]
// In the fuzz-only build the probe's outputs are intentionally unread.
#[cfg_attr(
    all(feature = "fuzz-internals", not(any(av1_dav1d, test))),
    allow(dead_code)
)]
mod avis;
// Planar YUV→RGBA8 for the avis backend — pure math; the fuzz harness never
// reaches it (the probe is demux-only), so it isn't compiled there.
#[cfg(any(av1_dav1d, test))]
mod yuv;

/// Fuzz-harness hook (hidden; `fuzz/fuzz_targets/probe_avis.rs`): drives the
/// pure-Rust avis demuxer on arbitrary bytes. Never part of a real build.
#[cfg(feature = "fuzz-internals")]
#[doc(hidden)]
pub fn fuzz_probe_avis(bytes: &[u8]) {
    let _ = avis::probe_avis(bytes);
}
// Shared ISOBMFF/`colr` parsing for the HEVC/AV1 container backends (WIC, Image I/O,
// libheif). Only compiled where one of them is — keeps non-HEIC targets (e.g. a
// libheif-less Linux bench build) dead-code-free.
#[cfg(any(windows, target_os = "macos", heic_libheif))]
mod isobmff;
mod jxl;
// The CPU libheif HEVC backend: Windows (vcpkg static) and Linux (system shared
// lib). `heic_libheif` is set by build.rs when the `libheif` feature is on for a
// supported target — a single flag so the gates below don't repeat the cfg.
#[cfg(heic_libheif)]
mod libheif;
pub mod metadata;
pub mod orientation;
// Video playback frame contract (task #79 phase 0): the VideoFrame/VideoColorInfo
// types every platform video producer emits. Pure data — producers come later.
mod psd;
mod raw;
mod svg;
pub mod video;
#[cfg(windows)]
mod wic;
mod zune;

use std::path::Path;

pub use animation::{
    decode_animation, decode_animation_cancellable, detect_animation, AnimFrame, Animation,
    AnimationKind, MotionChunk, MotionHeader,
};
pub use color::ColorTransform;
#[cfg(all(unix, not(target_os = "macos"), feature = "livephoto"))]
pub use ff_live::{
    decode_image_sequence, decode_image_sequence_cancellable, decode_live_motion,
    decode_live_motion_cancellable, decode_live_motion_streaming, decode_motion_audio, MotionAudio,
};
pub use image_backend::ImageCrateDecoder;
#[cfg(target_os = "macos")]
pub use imageio::ImageIoDecoder;
pub use jxl::JxlDecoder;
#[cfg(heic_libheif)]
pub use libheif::LibHeifDecoder;
#[cfg(target_os = "macos")]
pub use livephoto::{decode_live_motion, decode_live_motion_streaming};
pub use metadata::read_exif_fields;
#[cfg(windows)]
pub use mf_video::{decode_live_motion, decode_live_motion_streaming};
pub use psd::PsdDecoder;
pub use raw::{is_raw_extension, RawPreviewDecoder};
pub use svg::SvgDecoder;
pub use video::{SeekGeneration, VideoColorInfo, VideoFrame, VideoSessionId};
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
    /// If the source container is animated (GIF/APNG/animated-WebP, or an AVIF/HEIC
    /// sequence on macOS), which kind — set by the caller from a cheap header sniff
    /// ([`detect_animation`]). `None` for a still. The decoded pixels are always just
    /// the first frame (the canonical still everywhere); this only flags that an
    /// on-demand multi-frame [`decode_animation`] playback is available (task #37).
    pub animated: Option<AnimationKind>,
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

/// Downscale a tightly-packed RGBA8 buffer to fit within `fit` (aspect preserved,
/// Lanczos3; never upscales — the input comes back unchanged when it already fits).
/// The decode paths use this internally via `finalize`; it's public for consumers
/// that post-process already-decoded pixels, e.g. capping OCR input to the engine's
/// max dimension (task #45).
pub fn downscale_rgba8(
    pixels: Vec<u8>,
    w: u32,
    h: u32,
    fit: FitBox,
) -> Result<(Vec<u8>, u32, u32), DecodeError> {
    common::downscale_to_fit(pixels, w, h, fit)
}

/// Encode a tightly-packed RGBA8 buffer to baseline JPEG at `quality` (1–100). Alpha is
/// dropped — JPEG has none, and the caller (the AI-describe image prep, task #44) feeds
/// opaque, already-tone-mapped sRGB8 pixels, so there's nothing to composite. Used only to
/// shrink an image for an OpenAI-compatible endpoint's base64 data URI — never on the view
/// path (privacy #2: an explicit describe command, not a passive byproduct of viewing).
pub fn encode_jpeg_rgba8(rgba: &[u8], w: u32, h: u32, quality: u8) -> Result<Vec<u8>, DecodeError> {
    let expected = (w as usize) * (h as usize) * 4;
    if w == 0 || h == 0 || rgba.len() != expected {
        return Err(DecodeError::Corrupt(format!(
            "encode_jpeg_rgba8: {w}x{h} needs {expected} bytes, got {}",
            rgba.len()
        )));
    }
    // RGBA → RGB (JPEG is 3-channel); the describe pixels are opaque.
    let mut rgb = Vec::with_capacity((w as usize) * (h as usize) * 3);
    for px in rgba.chunks_exact(4) {
        rgb.extend_from_slice(&px[..3]);
    }
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality.clamp(1, 100))
        .encode(&rgb, w, h, image::ExtendedColorType::Rgb8)
        .map_err(|e| DecodeError::Corrupt(format!("jpeg encode failed: {e}")))?;
    Ok(out)
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
            animated: None,
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
    // HEIC decodes go to the CPU libheif backend when it's built in. On Windows it's
    // an A/B accelerator over WIC (WIC's HEVC decoder serializes; libheif scales
    // across cores) — AVIF / HDR HEIC / thumbnailed previews fall through to WIC
    // below. On Linux there is no WIC, so libheif is the *only* HEIC path (and it
    // also takes the previews `route_full_heic` would otherwise leave to WIC).
    #[cfg(heic_libheif)]
    if libheif::route_full_heic(bytes, allow_preview) {
        return libheif::LibHeifDecoder.decode(&req);
    }
    // Specific magic-sniffable backends first, the broad `image` crate last.
    let jpeg = ZuneJpegDecoder;
    let jxl = JxlDecoder;
    let psd = PsdDecoder;
    let images = ImageCrateDecoder;
    // PSD is `8BPS`-sniffable, so it routes by content like JPEG/JXL — no
    // extension hint needed (the `image` crate doesn't handle PSD anyway).
    let mut backends: Vec<&dyn ImageDecoder> = vec![&jpeg, &jxl, &psd];
    // AVIF + HEIC via the OS imaging codecs (pure-Rust elsewhere can't do AV1/HEVC):
    // WIC on Windows, Image I/O on macOS. Tried before the `image` crate, which
    // doesn't handle them here.
    #[cfg(windows)]
    let wic = WicDecoder;
    #[cfg(windows)]
    backends.push(&wic);
    #[cfg(target_os = "macos")]
    let imageio = ImageIoDecoder;
    #[cfg(target_os = "macos")]
    backends.push(&imageio);
    backends.push(&images);
    for backend in backends {
        if backend.can_decode(bytes) {
            return backend.decode(&req);
        }
    }
    Err(DecodeError::Unsupported)
}

/// Decode in-memory image `bytes` the way [`decode_image_file`] would, but taking
/// `name` (a file name or path — anything carrying an extension) purely as the
/// routing hint for the formats a content-sniff can't identify: RAW (TIFF-shaped),
/// SVG (XML text), and TGA (no magic number). Everything else falls through to the
/// content-sniffing [`decode_bytes`].
///
/// This is the entry point for sources whose bytes don't live in a standalone file
/// — an image *inside a ZIP archive*, say, where the entry name carries the
/// extension but `std::fs::read` never runs. It is the in-memory twin of
/// [`decode_image_file`]; that function is now just `fs::read` + this.
///
/// **Panic-safe** in the same way as [`decode_image_file`].
pub fn decode_named_bytes(
    name: &str,
    bytes: &[u8],
    fit: Option<FitBox>,
    allow_preview: bool,
) -> Result<DecodedImage, DecodeError> {
    catch_panics(|| decode_named_bytes_inner(name, bytes, fit, allow_preview))
}

fn decode_named_bytes_inner(
    name: &str,
    bytes: &[u8],
    fit: Option<FitBox>,
    allow_preview: bool,
) -> Result<DecodedImage, DecodeError> {
    let ext = ext_of(name);
    let req = DecodeRequest {
        bytes,
        fit,
        allow_preview,
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
        return image_backend::decode_tga(bytes, fit);
    }
    decode_bytes_inner(bytes, fit, allow_preview)
}

/// The lowercased extension (no dot) of a file name or path, or `""` if none.
/// Uses `Path` so only the last path component's extension counts (e.g. a ZIP
/// entry name like `trip/day1/IMG.TGA` → `tga`).
fn ext_of(name: &str) -> String {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default()
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
pub fn decode_image_file(
    path: &Path,
    fit: Option<FitBox>,
    allow_preview: bool,
) -> Result<DecodedImage, DecodeError> {
    catch_panics(|| decode_image_file_inner(path, fit, allow_preview))
}

fn decode_image_file_inner(
    path: &Path,
    fit: Option<FitBox>,
    allow_preview: bool,
) -> Result<DecodedImage, DecodeError> {
    let bytes =
        std::fs::read(path).map_err(|e| DecodeError::Corrupt(format!("read error: {e}")))?;
    // The path's file name carries the extension the ambiguous formats route on;
    // everything else is content-sniffed. Shared with the archive path via
    // `decode_named_bytes_inner` so the routing has one home.
    decode_named_bytes_inner(&path.to_string_lossy(), &bytes, fit, allow_preview)
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
        // Photoshop (embedded flattened composite). `psb`/Large Document Format
        // is a distinct variant the `psd` crate doesn't parse — excluded for v1.
        "psd", //
        // RAW (embedded-preview path)
        "arw", "nef", "cr2", "cr3", "dng", "raf", "rw2", "orf", "srw", "pef", "raw",
    ];
    if BASE.contains(&e.as_str()) {
        return true;
    }
    // AVIF/HEIC are decoded via the OS codecs: WIC on Windows, Image I/O on macOS.
    #[cfg(any(windows, target_os = "macos"))]
    if matches!(e.as_str(), "avif" | "heic" | "heif" | "hif") {
        return true;
    }
    // On Linux, HEIC comes via the system libheif when the `libheif` feature is on — and
    // AVIF too when that libheif carries an AV1 decoder (the usual dav1d build). Without
    // this arm the *scanner* skipped these even though the decoder could open them —
    // folders full of iPhone HEICs / AVIFs appeared empty.
    #[cfg(all(heic_libheif, not(any(windows, target_os = "macos"))))]
    {
        if matches!(e.as_str(), "heic" | "heif" | "hif") {
            return true;
        }
        if e.as_str() == "avif" && libheif::has_av1_decoder() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_jpeg_rgba8_produces_a_valid_jpeg_and_round_trips() {
        // A 4×4 solid mid-gray tile.
        let (w, h) = (4u32, 4u32);
        let rgba: Vec<u8> = std::iter::repeat_n([128u8, 130, 132, 255], (w * h) as usize)
            .flatten()
            .collect();
        let jpeg = encode_jpeg_rgba8(&rgba, w, h, 85).expect("encode");
        // JPEG SOI/EOI markers frame a real JFIF stream.
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "SOI");
        assert_eq!(&jpeg[jpeg.len() - 2..], &[0xFF, 0xD9], "EOI");
        // Decodes back to the same dimensions (colors shift under lossy compression).
        let back = zune_jpeg::JpegDecoder::new(&jpeg);
        let mut back = back;
        let px = back.decode().expect("decode back");
        let info = back.info().expect("info");
        assert_eq!((info.width as u32, info.height as u32), (w, h));
        assert!(!px.is_empty());
    }

    #[test]
    fn encode_jpeg_rgba8_rejects_a_size_mismatch() {
        assert!(encode_jpeg_rgba8(&[0; 3], 4, 4, 85).is_err());
        assert!(encode_jpeg_rgba8(&[], 0, 0, 85).is_err());
    }

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
        assert!(is_supported_extension("psd"));
        assert!(is_supported_extension("PSD"));
        // .psb (Large Document Format) is not parsed by the psd crate yet — v1 psd-only.
        assert!(!is_supported_extension("psb"));
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
        let decoded = decode_image_file(&path, None, false);
        let _ = std::fs::remove_file(&path);
        let img = decoded.expect("tga decode");
        assert_eq!(img.codec, "TGA");
        assert_eq!((img.orig_width, img.orig_height), (3, 2));
        assert!(img.is_well_formed());
    }

    #[test]
    fn named_bytes_routes_tga_by_name_hint() {
        // TGA can't be content-sniffed; from a bare byte slice it decodes only when
        // the caller supplies a name carrying the .tga extension — the ZIP-entry
        // case, where there is no file path to route on.
        let bytes = encode(image::ImageFormat::Tga);
        let img = decode_named_bytes("photo.tga", &bytes, None, false).expect("tga decode");
        assert_eq!(img.codec, "TGA");
        assert_eq!((img.orig_width, img.orig_height), (3, 2));
        assert!(img.is_well_formed());
    }

    #[test]
    fn named_bytes_uses_only_the_last_component_extension() {
        // An archive-relative hint may carry directories; only the final
        // component's extension (case-insensitive) decides routing.
        let bytes = encode(image::ImageFormat::Tga);
        let img = decode_named_bytes("trip/day1/IMG.TGA", &bytes, None, false).expect("tga");
        assert_eq!(img.codec, "TGA");
    }

    #[test]
    fn named_bytes_falls_back_to_content_sniff_when_name_is_unhelpful() {
        // A magic-sniffable format still decodes when the name has no extension.
        let bytes = encode(image::ImageFormat::Png);
        let img = decode_named_bytes("noextension", &bytes, None, false).expect("png");
        assert_eq!(img.codec, "PNG");
    }

    #[test]
    fn named_bytes_routes_svg_by_name() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="8"><rect width="10" height="8" fill="red"/></svg>"#;
        let img = decode_named_bytes("logo.svg", svg, None, false).expect("svg");
        assert_eq!(img.codec, "SVG");
        assert_eq!((img.orig_width, img.orig_height), (10, 8));
    }

    #[test]
    fn ext_of_extracts_lowercased_last_extension() {
        assert_eq!(ext_of("a.JPG"), "jpg");
        assert_eq!(ext_of("trip/day1/IMG.tga"), "tga");
        assert_eq!(ext_of("noext"), "");
        assert_eq!(ext_of(".hidden"), "");
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
