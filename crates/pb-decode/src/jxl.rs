//! JPEG XL backend: `jxl-oxide` (pure Rust, no system deps).
//!
//! Renders the first keyframe to interleaved f32 channels, then quantizes to
//! RGBA8. Animation is ignored (first frame only) — this is a still viewer.
//!
//! A lossy (XYB) image in a wide gamut is the exception (task #156): jxl-oxide would squeeze
//! it through sRGB, so it is rendered in linear sRGB and re-encoded here. See [`WideGamut`].

use std::io::Cursor;

use jxl_oxide::{EnumColourEncoding, JxlImage, JxlThreadPool, PixelFormat, RenderingIntent};

use crate::color::{has_srgb_gamut, SrgbLinearEncoder};
use crate::{common, ColorTransform, DecodeError, DecodeRequest, DecodedImage, ImageDecoder};

/// JXL signature box that opens a container-format `.jxl` file.
const JXL_BOX_SIG: [u8; 12] = [
    0x00, 0x00, 0x00, 0x0C, 0x4A, 0x58, 0x4C, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
];

#[derive(Debug, Clone, Copy, Default)]
pub struct JxlDecoder;

impl ImageDecoder for JxlDecoder {
    fn can_decode(&self, bytes: &[u8]) -> bool {
        // Bare codestream (FF 0A) or the ISOBMFF container signature box.
        bytes.starts_with(&[0xFF, 0x0A]) || bytes.starts_with(&JXL_BOX_SIG)
    }

    fn decode(&self, req: &DecodeRequest) -> Result<DecodedImage, DecodeError> {
        let mut image = JxlImage::read_with_defaults(Cursor::new(req.bytes))
            .map_err(|e| DecodeError::Corrupt(e.to_string()))?;
        let (w, h) = (image.width(), image.height());
        let fmt = image.pixel_format();
        let wide = WideGamut::request(&mut image);
        let render = image
            .render_frame(0)
            .map_err(|e| DecodeError::Corrupt(e.to_string()))?;
        let fb = render.image_all_channels();
        let ch = fb.channels();
        let n = (w as usize) * (h as usize);
        if ch < fmt.channels() || fb.buf().len() < n * ch {
            return Err(DecodeError::Corrupt("jxl framebuffer too small".into()));
        }
        let (rgba, color) = match wide {
            Some(wide) => {
                let rgba = wide.convert(
                    &fb.buf()[..n * ch],
                    ch,
                    4,
                    fmt.has_alpha(),
                    image.pool(),
                    SrgbLinearEncoder::encode8,
                    quantize8,
                );
                (rgba, wide.color)
            }
            // jxl-oxide renders in the image's own color space; `rendered_icc` is the
            // profile describing exactly the buffer we just quantized. Feed it to the
            // in-shader color path (sRGB images resolve to a no-op passthrough).
            None => (
                to_rgba8(fb.buf(), n, ch, fmt)?,
                ColorTransform::from_icc(&image.rendered_icc()),
            ),
        };
        let mut img = common::finalize(rgba, w, h, None, "JXL", req.fit, false)?;
        img.color = color;
        Ok(img)
    }

    fn name(&self) -> &'static str {
        "jxl-oxide"
    }
}

/// The JPEG XL **export** decode (rung 0b): SDR at the image's own bit depth (8 or 16) with
/// the rendered ICC — the display decode quantizes everything to 8-bit. HDR JPEG XL is
/// refused until rung 5 adds its linearization (review C18).
pub(crate) fn export_jxl(
    bytes: &[u8],
) -> Result<pb_encode::ExportSource, crate::export::ExportDecodeError> {
    use pb_encode::{AlphaMode, Channels, ExportSource, Provenance, SampleEncoding, Samples};
    let mut image = JxlImage::read_with_defaults(Cursor::new(bytes))
        .map_err(|e| DecodeError::Corrupt(e.to_string()))?;
    if image.hdr_type().is_some() {
        return Err(crate::export::ExportDecodeError::Refused(
            "HDR JPEG XL images can't be exported yet",
        ));
    }
    let (w, h) = (image.width(), image.height());
    let fmt = image.pixel_format();
    let (channels, take) = match fmt {
        PixelFormat::Gray => (Channels::Gray, 1),
        PixelFormat::Graya => (Channels::GrayAlpha, 2),
        PixelFormat::Rgb => (Channels::Rgb, 3),
        PixelFormat::Rgba => (Channels::Rgba, 4),
        PixelFormat::Cmyk | PixelFormat::Cmyka => {
            return Err(crate::export::ExportDecodeError::Refused(
                "CMYK images can't be exported yet",
            ))
        }
    };
    let bits = image.image_header().metadata.bit_depth.bits_per_sample();
    let wide = WideGamut::request(&mut image);
    let render = image
        .render_frame(0)
        .map_err(|e| DecodeError::Corrupt(e.to_string()))?;
    let fb = render.image_all_channels();
    let ch = fb.channels();
    let n = (w as usize) * (h as usize);
    if ch < take || fb.buf().len() < n * ch {
        return Err(DecodeError::Corrupt("jxl framebuffer too small".into()).into());
    }
    let buf = &fb.buf()[..n * ch];
    let samples = match (&wide, bits <= 8) {
        (Some(wide), true) => Samples::U8(wide.convert(
            buf,
            ch,
            take,
            take == 4,
            image.pool(),
            SrgbLinearEncoder::encode8,
            quantize8,
        )),
        (Some(wide), false) => Samples::U16(wide.convert(
            buf,
            ch,
            take,
            take == 4,
            image.pool(),
            SrgbLinearEncoder::encode16,
            quantize16,
        )),
        (None, true) => Samples::U8(
            (0..n)
                .flat_map(|i| (0..take).map(move |c| quantize8(buf[i * ch + c])))
                .collect(),
        ),
        (None, false) => Samples::U16(
            (0..n)
                .flat_map(|i| (0..take).map(move |c| quantize16(buf[i * ch + c])))
                .collect(),
        ),
    };
    // After a wide-gamut request, `rendered_icc` describes linear sRGB, not these samples.
    let icc = match wide {
        Some(wide) => wide.icc,
        None => image.rendered_icc(),
    };
    Ok(ExportSource {
        width: w,
        height: h,
        channels,
        samples,
        encoding: SampleEncoding::Encoded,
        alpha: AlphaMode::Straight,
        color: crate::export::icc_desc(Some(icc)),
        provenance: Provenance {
            codec: "JXL",
            path: "jxl-oxide",
            recovered: false,
            raw_preview: false,
        },
    })
}

/// Lossy JPEG XL in a wide gamut, rendered without jxl-oxide's squeeze (task #156).
///
/// A lossy JPEG XL stores XYB, and jxl-oxide (0.12; jxl-color 0.11 `convert.rs`) converts
/// XYB to linear sRGB first and then to the image's own space. When that space's primaries
/// or white point aren't sRGB's, it inserts a gamut map (jxl-render hard-codes the
/// perceptual intent for XYB frames, whatever the header says) or a clip, *before* the
/// matrix to the wider space: Display P3 red came out about (216, 55, 55), where libjxl,
/// Preview and Safari keep it (255, 0, 0). No request avoids that while the target's
/// primaries differ, and a CMS doesn't help (the XYB-to-enum path never calls one).
///
/// So: ask jxl-oxide for **linear sRGB**, whose primaries match, so it adds no gamut step
/// and hands out extended-range floats (P3 red is about (1.22, -0.04, -0.02)). Then
/// re-encode into the image's own space with [`SrgbLinearEncoder`], the inverse of the
/// colour transform the renderer applies, and display through that transform as before.
/// The work jxl-oxide skips (the gamut map and its transfer curve) pays for ours.
///
/// Only lossy SDR RGB images take this path. Lossless ones keep their original space and
/// never convert; sRGB, grey and CMYK have no wider gamut to lose; HDR (PQ, HLG, or an
/// intensity target above 255 nits, which jxl-oxide tone-maps and then gamut-maps anyway)
/// stays on its own path.
struct WideGamut {
    /// The image's own space as the renderer's transform.
    color: ColorTransform,
    /// The image's own profile, which the export decode tags its samples with.
    icc: Vec<u8>,
    encoder: SrgbLinearEncoder,
}

impl WideGamut {
    /// Requests linear sRGB from `image` when it needs this path; otherwise returns `None`
    /// and leaves the render untouched.
    fn request(image: &mut JxlImage) -> Option<Self> {
        let meta = &image.image_header().metadata;
        let sdr = image.hdr_type().is_none() && meta.tone_mapping.intensity_target <= 255.0;
        let rgb = matches!(image.pixel_format(), PixelFormat::Rgb | PixelFormat::Rgba);
        if !(meta.xyb_encoded && sdr && rgb) {
            return None;
        }
        let icc = image.rendered_icc();
        let color = ColorTransform::from_icc(&icc);
        if !color.enabled || has_srgb_gamut(&color) {
            return None;
        }
        let encoder = SrgbLinearEncoder::new(&color)?;
        image.request_color_encoding(EnumColourEncoding::srgb_linear(RenderingIntent::Relative));
        Some(Self {
            color,
            icc,
            encoder,
        })
    }

    /// Re-encodes the linear-sRGB framebuffer `buf` (stride `ch`: colour, then alpha) into
    /// `out_ch` samples per pixel in the image's own space: three colour channels through
    /// `encode`, then for four channels the alpha through `quantize` (opaque without one).
    /// Runs on jxl-oxide's thread pool, as its own colour conversion would have.
    #[allow(clippy::too_many_arguments)]
    fn convert<T: Copy + Default + Send + Sync>(
        &self,
        buf: &[f32],
        ch: usize,
        out_ch: usize,
        has_alpha: bool,
        pool: &JxlThreadPool,
        encode: fn(&SrgbLinearEncoder, f32) -> T,
        quantize: fn(f32) -> T,
    ) -> Vec<T> {
        /// Pixels per job: enough to amortize the dispatch, few enough to balance the load.
        const JOB_PX: usize = 32 * 1024;
        let n = buf.len() / ch;
        let opaque = quantize(1.0);
        let mut out = vec![T::default(); n * out_ch];
        let jobs: Vec<_> = out.chunks_mut(JOB_PX * out_ch).enumerate().collect();
        pool.for_each_vec(jobs, |(job, dst)| {
            let src = &buf[job * JOB_PX * ch..];
            for (px, s) in dst.chunks_exact_mut(out_ch).zip(src.chunks_exact(ch)) {
                let lin = self.encoder.to_target_linear([s[0], s[1], s[2]]);
                px[0] = encode(&self.encoder, lin[0]);
                px[1] = encode(&self.encoder, lin[1]);
                px[2] = encode(&self.encoder, lin[2]);
                if out_ch == 4 {
                    px[3] = if has_alpha { quantize(s[3]) } else { opaque };
                }
            }
        });
        out
    }
}

fn quantize8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn quantize16(v: f32) -> u16 {
    (v.clamp(0.0, 1.0) * 65535.0).round() as u16
}

/// Quantize interleaved f32 channels (stride `ch`, the framebuffer's channel
/// count) to RGBA8, mapping by the image's `PixelFormat`. Color channels come
/// first, then alpha, then any extra channels (ignored).
fn to_rgba8(buf: &[f32], n: usize, ch: usize, fmt: PixelFormat) -> Result<Vec<u8>, DecodeError> {
    let q = quantize8;
    let mut out = vec![0u8; n * 4];
    for i in 0..n {
        let s = &buf[i * ch..i * ch + ch];
        let px = match fmt {
            PixelFormat::Gray => {
                let g = q(s[0]);
                [g, g, g, 255]
            }
            PixelFormat::Graya => {
                let g = q(s[0]);
                [g, g, g, q(s[1])]
            }
            PixelFormat::Rgb => [q(s[0]), q(s[1]), q(s[2]), 255],
            PixelFormat::Rgba => [q(s[0]), q(s[1]), q(s[2]), q(s[3])],
            PixelFormat::Cmyk | PixelFormat::Cmyka => {
                return Err(DecodeError::Corrupt("JXL CMYK unsupported".into()));
            }
        };
        out[i * 4..i * 4 + 4].copy_from_slice(&px);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use moxcms::ParametricCurve;
    use pb_encode::{ColorDesc, Samples};

    /// The cards' patches, in each card's own space. Keep in step with `make_patches.py`.
    const PATCHES: [[u8; 3]; 6] = [
        [255, 0, 0],
        [0, 255, 0],
        [0, 0, 255],
        [255, 255, 255],
        [128, 128, 128],
        [230, 140, 60],
    ];
    const PATCH: u32 = 32;
    const ALPHA: u8 = 200;

    /// A card from `tests/fixtures/` (`jxl/` unless the name has a folder).
    fn fixture(name: &str) -> Vec<u8> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let path = if name.contains('/') {
            dir.join(name)
        } else {
            dir.join("jxl").join(name)
        };
        std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    fn display_decode(name: &str) -> DecodedImage {
        let bytes = fixture(name);
        JxlDecoder
            .decode(&DecodeRequest {
                bytes: &bytes,
                fit: None,
                allow_preview: false,
            })
            .unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    /// The RGBA8 texel at the centre of patch `k`.
    fn patch_px(img: &DecodedImage, k: usize) -> [u8; 4] {
        let (x, y) = (PATCH * k as u32 + PATCH / 2, PATCH / 2);
        let i = ((y * img.width + x) * 4) as usize;
        img.pixels[i..i + 4].try_into().unwrap()
    }

    /// What the renderer shows for an encoded colour: the in-shader path, linear sRGB with
    /// no gamut clamp (the fp16 scRGB intermediate). A disabled transform is a passthrough,
    /// so its texels are sRGB.
    fn shown(ct: &ColorTransform, rgb: [f32; 3]) -> [f32; 3] {
        let trc = if ct.enabled {
            ct.trc
        } else {
            [
                2.4,
                1.0 / 1.055,
                0.055 / 1.055,
                1.0 / 12.92,
                0.04045,
                0.0,
                0.0,
            ]
        };
        let [g, a, b, c, d, e, f] = trc;
        let eotf = ParametricCurve {
            g,
            a,
            b,
            c,
            d,
            e,
            f,
        };
        let lin = rgb.map(|v| eotf.eval(v));
        if !ct.enabled {
            return lin;
        }
        let m = ct.matrix;
        [0, 1, 2].map(|i| m[i][0] * lin[0] + m[i][1] * lin[1] + m[i][2] * lin[2])
    }

    fn shown8(ct: &ColorTransform, px: [u8; 3]) -> [f32; 3] {
        shown(ct, px.map(|v| v as f32 / 255.0))
    }

    /// Display P3 as the in-shader transform: the card's intended look.
    fn p3() -> ColorTransform {
        let ct = ColorTransform::from_cicp(12, 13, 0, true);
        assert!(ct.enabled, "P3 must not be a passthrough");
        ct
    }

    #[track_caller]
    fn assert_shows(label: &str, got: [f32; 3], want: [f32; 3], tol: f32) {
        let off = (0..3).map(|i| (got[i] - want[i]).abs()).fold(0.0, f32::max);
        assert!(
            off <= tol,
            "{label}: shows linear {got:?}, want {want:?} (off by {off:.4})"
        );
    }

    /// The decode before task #156, for the images it must not change: jxl-oxide's default
    /// render, quantized, with the rendered profile as the colour.
    fn old_display_decode(bytes: &[u8]) -> (Vec<u8>, ColorTransform) {
        let image = JxlImage::read_with_defaults(Cursor::new(bytes)).unwrap();
        let n = (image.width() * image.height()) as usize;
        let fb = image.render_frame(0).unwrap().image_all_channels();
        let rgba = to_rgba8(fb.buf(), n, fb.channels(), image.pixel_format()).unwrap();
        (rgba, ColorTransform::from_icc(&image.rendered_icc()))
    }

    /// Task #156: a lossy (XYB) JPEG XL of a Display P3 image keeps its gamut. jxl-oxide
    /// gamut-maps to sRGB on the way to P3, which turned P3 red into about (216, 55, 55).
    #[test]
    fn lossy_p3_jxl_keeps_its_wide_gamut() {
        let img = display_decode("p3-lossy.jxl");
        assert!(img.color.enabled, "a P3 image needs its colour transform");
        let p3 = p3();
        for (k, &code) in PATCHES.iter().enumerate() {
            let px = patch_px(&img, k);
            assert_eq!(px[3], 255, "patch {k}: opaque");
            let got = shown8(&img.color, [px[0], px[1], px[2]]);
            assert_shows(&format!("patch {k} {code:?}"), got, shown8(&p3, code), 0.02);
        }
        let red = patch_px(&img, 0);
        assert!(
            red[0] >= 252 && red[1] <= 4 && red[2] <= 4,
            "P3 red decodes as {red:?}, not (255, 0, 0)"
        );
    }

    #[test]
    fn lossy_p3_jxl_with_alpha_keeps_gamut_and_alpha() {
        let img = display_decode("p3-alpha-lossy.jxl");
        let p3 = p3();
        for (k, &code) in PATCHES.iter().enumerate() {
            let px = patch_px(&img, k);
            assert_eq!(px[3], ALPHA, "patch {k}: alpha");
            let got = shown8(&img.color, [px[0], px[1], px[2]]);
            assert_shows(&format!("patch {k} {code:?}"), got, shown8(&p3, code), 0.02);
        }
    }

    /// The controls: lossless keeps the original space (no conversion to squeeze), sRGB has
    /// no wider gamut to lose, and the HDR cards stay on their own path. Each decodes
    /// exactly as it did before task #156.
    #[test]
    fn lossless_srgb_and_hdr_jxl_decode_exactly_as_before() {
        for name in [
            "p3-lossless.jxl",
            "srgb-lossy.jxl",
            "hdr/pq-card.jxl",
            "hdr/hlg-card.jxl",
        ] {
            let img = display_decode(name);
            let (pixels, color) = old_display_decode(&fixture(name));
            assert!(img.pixels == pixels, "{name}: pixels changed");
            assert_eq!(img.color, color, "{name}: colour changed");
        }
        // And the controls are right, not merely unchanged.
        let img = display_decode("p3-lossless.jxl");
        assert_eq!(patch_px(&img, 0), [255, 0, 0, 255], "lossless P3 red");
        let p3 = p3();
        for (k, &code) in PATCHES.iter().enumerate() {
            let px = patch_px(&img, k);
            let got = shown8(&img.color, [px[0], px[1], px[2]]);
            assert_shows(&format!("lossless {k}"), got, shown8(&p3, code), 1e-3);
        }
        let img = display_decode("srgb-lossy.jxl");
        assert!(!img.color.enabled, "sRGB is a passthrough");
        let srgb = ColorTransform::srgb();
        for (k, &code) in PATCHES.iter().enumerate() {
            let px = patch_px(&img, k);
            let got = shown8(&srgb, [px[0], px[1], px[2]]);
            assert_shows(&format!("sRGB {k}"), got, shown8(&srgb, code), 0.02);
        }
    }

    /// The export decode keeps the gamut too, at the image's own bit depth, tagged with the
    /// image's own profile.
    #[test]
    fn export_keeps_lossy_p3_gamut_at_8_and_16_bits() {
        let p3 = p3();
        for (name, max) in [("p3-lossy.jxl", 255.0), ("p3-16-lossy.jxl", 65535.0)] {
            let src = crate::decode_named_bytes_for_export(name, &fixture(name))
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            let ColorDesc::Icc(icc) = &src.color else {
                panic!("{name}: colour {:?}", src.color);
            };
            let ct = ColorTransform::from_icc(icc);
            let samples: Vec<f32> = match &src.samples {
                Samples::U8(s) if max == 255.0 => s.iter().map(|&v| v as f32 / max).collect(),
                Samples::U16(s) if max == 65535.0 => s.iter().map(|&v| v as f32 / max).collect(),
                _ => panic!("{name}: wrong sample depth"),
            };
            assert_eq!(src.channels.count(), 3, "{name}: RGB");
            for (k, &code) in PATCHES.iter().enumerate() {
                let (x, y) = (PATCH * k as u32 + PATCH / 2, PATCH / 2);
                let i = ((y * src.width + x) * 3) as usize;
                let got = shown(&ct, [samples[i], samples[i + 1], samples[i + 2]]);
                assert_shows(&format!("{name} {k}"), got, shown8(&p3, code), 0.02);
            }
            let red = &samples[((PATCH / 2 * src.width + PATCH / 2) * 3) as usize..][..3];
            assert!(
                red[0] >= 0.985 && red[1] <= 0.015 && red[2] <= 0.015,
                "{name}: P3 red exports as {red:?}"
            );
        }
    }

    #[test]
    fn sniffs_jxl_signatures() {
        let d = JxlDecoder;
        assert!(d.can_decode(&[0xFF, 0x0A, 0x00])); // bare codestream
        assert!(d.can_decode(&JXL_BOX_SIG)); // container
        assert!(!d.can_decode(&[0xFF, 0xD8, 0xFF])); // JPEG
        assert!(!d.can_decode(&[0x89, b'P', b'N', b'G']));
    }

    /// Full-resolution display-decode time (task #156), p50 / p95 / max over `PB_JXL_RUNS`
    /// runs (default 9) after one warm-up, for each `:`-separated path in `PB_JXL_BENCH`:
    ///
    /// `PB_JXL_BENCH=a.jxl:b.jxl cargo test --release -p pb-decode jxl::tests::decode_timing -- --ignored --nocapture`
    #[test]
    #[ignore = "a measurement: set PB_JXL_BENCH, run with --release --ignored --nocapture"]
    fn decode_timing() {
        let Ok(paths) = std::env::var("PB_JXL_BENCH") else {
            eprintln!("PB_JXL_BENCH is not set; nothing to time");
            return;
        };
        let runs: usize = std::env::var("PB_JXL_RUNS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(9);
        for path in paths.split(':').filter(|p| !p.is_empty()) {
            let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
            let req = DecodeRequest {
                bytes: &bytes,
                fit: None,
                allow_preview: false,
            };
            let first = JxlDecoder.decode(&req).expect("decode");
            let mut ms: Vec<f64> = (0..runs)
                .map(|_| {
                    let t = std::time::Instant::now();
                    let img = JxlDecoder.decode(&req).expect("decode");
                    let dt = t.elapsed().as_secs_f64() * 1e3;
                    std::hint::black_box(img);
                    dt
                })
                .collect();
            ms.sort_by(f64::total_cmp);
            let at = |q: f64| ms[((ms.len() - 1) as f64 * q).round() as usize];
            // FNV-1a over the pixels: compares outputs across builds without keeping them.
            let sum = first.pixels.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
                (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
            });
            println!(
                "{path}: {}x{} colour={} pixels={sum:016x} p50 {:.1} ms  p95 {:.1} ms  max {:.1} ms  ({runs} runs)",
                first.width,
                first.height,
                if first.color.enabled { "CMS" } else { "sRGB" },
                at(0.5),
                at(0.95),
                at(1.0),
            );
        }
    }
}
