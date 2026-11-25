//! **WebP** over libwebp (export-plan.md rung 4): `libwebp-sys` builds the vendored library
//! (BSD-3, with Google's VP8 patent grant) with `cc`, no system dependency. Lossy or lossless,
//! 8-bit with alpha; the colour profile goes in an `ICCP` chunk through libwebp's own mux
//! library (an sRGB image embeds none — untagged WebP is read as sRGB).
//!
//! Every libwebp object is created and freed inside one call: no handle outlives `encode`.

use std::ffi::{c_int, c_void};

use libwebp_sys as w;

use crate::encoder::{Caps, Encoder};
use crate::types::{ExportImage, OutputColor};
use crate::EncodeError;

/// WebP's limit: 14-bit dimensions.
pub const MAX_EDGE: u32 = 16_383;

/// The WebP export format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebpEncoder {
    /// 1–100 for lossy; ignored when `lossless`.
    pub quality: u8,
    pub lossless: bool,
}

impl Encoder for WebpEncoder {
    fn caps(&self) -> Caps {
        Caps::EIGHT_BIT_ALPHA
    }
    fn extension(&self) -> &'static str {
        "webp"
    }
    fn lossless(&self) -> bool {
        self.lossless
    }
    fn encode(&self, img: &ExportImage) -> Result<Vec<u8>, EncodeError> {
        encode_webp(img, self.quality, self.lossless)
    }
}

fn codec(why: impl std::fmt::Display) -> EncodeError {
    EncodeError::Codec(format!("webp: {why}"))
}

/// libwebp's lossless effort (its `quality` in lossless mode: 0 fastest – 100 smallest).
/// Chosen from corpus timings (export-plan.md rung 4) — the preview waits for it.
pub const LOSSLESS_EFFORT: u8 = 50;

/// Encode a prepared 8-bit image as WebP.
pub fn encode_webp(img: &ExportImage, quality: u8, lossless: bool) -> Result<Vec<u8>, EncodeError> {
    encode_webp_at(img, quality, lossless, LOSSLESS_EFFORT)
}

/// [`encode_webp`] at another lossless `effort` (0–100) — for measuring, not the app.
pub fn encode_webp_at(
    img: &ExportImage,
    quality: u8,
    lossless: bool,
    effort: u8,
) -> Result<Vec<u8>, EncodeError> {
    let (px, alpha) = crate::px::rgb8(img)?;
    if img.width > MAX_EDGE || img.height > MAX_EDGE {
        return Err(EncodeError::Codec(format!(
            "WebP images can be at most {MAX_EDGE} pixels on a side"
        )));
    }
    let bitstream =
        unsafe { encode_bitstream(&px, img.width, img.height, alpha, quality, lossless, effort) }?;
    match &img.color {
        OutputColor::Srgb => Ok(bitstream),
        OutputColor::Icc(profile) => unsafe { with_icc(&bitstream, profile) },
    }
}

/// libwebp's advanced API: one config, one picture, a memory writer.
unsafe fn encode_bitstream(
    px: &[u8],
    width: u32,
    height: u32,
    alpha: bool,
    quality: u8,
    lossless: bool,
    effort: u8,
) -> Result<Vec<u8>, EncodeError> {
    let abi = w::WEBP_ENCODER_ABI_VERSION as c_int;
    let q = quality.clamp(1, 100) as f32;

    let mut config = std::mem::MaybeUninit::<w::WebPConfig>::zeroed();
    if unsafe {
        w::WebPConfigInitInternal(
            config.as_mut_ptr(),
            w::WebPPreset::WEBP_PRESET_DEFAULT,
            q,
            abi,
        )
    } == 0
    {
        return Err(codec("library version mismatch"));
    }
    let mut config = unsafe { config.assume_init() };
    config.method = 4; // libwebp's default speed/size balance
    config.thread_level = 1; // one helper thread
    if lossless {
        config.lossless = 1;
        config.quality = effort.min(100) as f32; // for lossless, `quality` is the effort
                                                 // Keep the colour under fully transparent pixels: the file must decode to exactly the
                                                 // pixels it was given (`prepare` already cleared them when Strip metadata is on).
        config.exact = 1;
    } else {
        config.alpha_quality = 100;
        config.use_sharp_yuv = 1; // sharper colour edges for 4:2:0, worth its small cost
    }
    if unsafe { w::WebPValidateConfig(&config) } == 0 {
        return Err(codec("invalid settings"));
    }

    let mut pic = std::mem::MaybeUninit::<w::WebPPicture>::zeroed();
    if unsafe { w::WebPPictureInitInternal(pic.as_mut_ptr(), abi) } == 0 {
        return Err(codec("library version mismatch"));
    }
    let mut pic = unsafe { pic.assume_init() };
    pic.width = width as c_int;
    pic.height = height as c_int;
    pic.use_argb = lossless as c_int;
    let imported = unsafe {
        if alpha {
            w::WebPPictureImportRGBA(&mut pic, px.as_ptr(), (width * 4) as c_int)
        } else {
            w::WebPPictureImportRGB(&mut pic, px.as_ptr(), (width * 3) as c_int)
        }
    };
    if imported == 0 {
        unsafe { w::WebPPictureFree(&mut pic) };
        return Err(codec("out of memory"));
    }

    let mut writer = std::mem::MaybeUninit::<w::WebPMemoryWriter>::zeroed();
    unsafe { w::WebPMemoryWriterInit(writer.as_mut_ptr()) };
    let mut writer = unsafe { writer.assume_init() };
    pic.writer = Some(w::WebPMemoryWrite);
    pic.custom_ptr = (&mut writer as *mut w::WebPMemoryWriter).cast::<c_void>();

    let ok = unsafe { w::WebPEncode(&config, &mut pic) };
    let error = pic.error_code;
    unsafe { w::WebPPictureFree(&mut pic) };
    let out = if ok == 0 || writer.mem.is_null() {
        Err(codec(format!("{error:?}")))
    } else {
        Ok(unsafe { std::slice::from_raw_parts(writer.mem, writer.size) }.to_vec())
    };
    unsafe { w::WebPMemoryWriterClear(&mut writer) };
    out
}

/// Add an `ICCP` chunk (making the file the extended `VP8X` layout) with libwebp's mux.
unsafe fn with_icc(bitstream: &[u8], profile: &[u8]) -> Result<Vec<u8>, EncodeError> {
    let image = w::WebPData {
        bytes: bitstream.as_ptr(),
        size: bitstream.len(),
    };
    let mux = unsafe { w::WebPMuxCreateInternal(&image, 1, w::WEBP_MUX_ABI_VERSION as c_int) };
    if mux.is_null() {
        return Err(codec("couldn't add the color profile"));
    }
    let icc = w::WebPData {
        bytes: profile.as_ptr(),
        size: profile.len(),
    };
    let set = unsafe { w::WebPMuxSetChunk(mux, c"ICCP".as_ptr(), &icc, 1) };
    let mut out = w::WebPData {
        bytes: std::ptr::null(),
        size: 0,
    };
    let assembled = if set == w::WebPMuxError::WEBP_MUX_OK {
        unsafe { w::WebPMuxAssemble(mux, &mut out) }
    } else {
        set
    };
    unsafe { w::WebPMuxDelete(mux) };
    if assembled != w::WebPMuxError::WEBP_MUX_OK || out.bytes.is_null() {
        return Err(codec(format!(
            "couldn't add the color profile ({assembled:?})"
        )));
    }
    let bytes = unsafe { std::slice::from_raw_parts(out.bytes, out.size) }.to_vec();
    unsafe { w::WebPFree(out.bytes as *mut c_void) };
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Channels, Samples};
    use image::ImageDecoder as _;

    fn image(w: u32, h: u32, channels: Channels, px: Vec<u8>, color: OutputColor) -> ExportImage {
        ExportImage {
            width: w,
            height: h,
            channels,
            samples: Samples::U8(px),
            color,
            notes: Vec::new(),
        }
    }

    fn gradient(w: u32, h: u32, n: usize) -> Vec<u8> {
        (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                let px = [(x * 255 / w) as u8, (y * 255 / h) as u8, 128, (x * 7) as u8];
                px.into_iter().take(n).collect::<Vec<_>>()
            })
            .collect()
    }

    fn decode(bytes: &[u8]) -> (image::DynamicImage, Option<Vec<u8>>) {
        let mut d = image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(bytes)).unwrap();
        let icc = d.icc_profile().unwrap();
        (image::DynamicImage::from_decoder(d).unwrap(), icc)
    }

    /// Lossless is exact — alpha and the colour under transparent pixels included — and the
    /// profile rides along byte for byte.
    #[test]
    fn lossless_is_exact_and_carries_the_profile() {
        let p3 = crate::icc::display_p3_profile();
        let px = gradient(40, 30, 4);
        let webp = encode_webp(
            &image(
                40,
                30,
                Channels::Rgba,
                px.clone(),
                OutputColor::Icc(p3.clone()),
            ),
            80,
            true,
        )
        .unwrap();
        assert_eq!(&webp[..4], b"RIFF");
        assert_eq!(
            &webp[8..16],
            b"WEBPVP8X",
            "the extended layout, for the ICCP chunk"
        );
        let (back, icc) = decode(&webp);
        assert_eq!(back.to_rgba8().into_raw(), px);
        assert_eq!(icc.as_deref(), Some(p3.as_slice()));
        assert!(WebpEncoder {
            quality: 80,
            lossless: true
        }
        .lossless());
    }

    /// Lossy decodes close to the source, smaller at lower quality; sRGB embeds nothing; gray
    /// is widened (WebP has no gray layout).
    #[test]
    fn lossy_is_close_and_quality_trades_size() {
        let px = gradient(64, 48, 3);
        let src = image(64, 48, Channels::Rgb, px.clone(), OutputColor::Srgb);
        let hi = encode_webp(&src, 90, false).unwrap();
        let lo = encode_webp(&src, 20, false).unwrap();
        assert!(lo.len() < hi.len(), "{} < {}", lo.len(), hi.len());
        let (back, icc) = decode(&hi);
        assert_eq!(icc, None);
        let back = back.to_rgb8().into_raw();
        let worst = back
            .iter()
            .zip(&px)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        assert!(worst < 24, "max error {worst}");

        let gray = image(8, 8, Channels::Gray, vec![77; 64], OutputColor::Srgb);
        let (back, _) = decode(&encode_webp(&gray, 90, false).unwrap());
        assert!(back
            .to_rgb8()
            .pixels()
            .all(|p| p.0.iter().all(|c| c.abs_diff(77) <= 2)));
    }

    #[test]
    fn refuses_what_it_cannot_write() {
        let too_wide = ExportImage {
            width: MAX_EDGE + 1,
            height: 1,
            channels: Channels::Gray,
            samples: Samples::U8(vec![0; MAX_EDGE as usize + 1]),
            color: OutputColor::Srgb,
            notes: Vec::new(),
        };
        assert!(matches!(
            encode_webp(&too_wide, 80, false),
            Err(EncodeError::Codec(_))
        ));
        let deep = ExportImage {
            samples: Samples::U16(vec![0; 3]),
            ..image(1, 1, Channels::Rgb, vec![], OutputColor::Srgb)
        };
        assert_eq!(encode_webp(&deep, 80, false), Err(EncodeError::Malformed));
    }
}
