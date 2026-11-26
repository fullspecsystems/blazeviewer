//! Export's codecs measured (task #89 rung 4): size, encode time and visual quality
//! (SSIMULACRA2) for every format across a quality grid, over the images in a folder — where
//! the default qualities, AVIF's speed and JPEG XL's effort come from.
//!
//!     cargo run --release -p pb-app-core --features export-codecs,avif-asm \
//!         --example codec_bench -- <folder>            # the quality grid at 2048 px
//!     ... --example codec_bench -- <folder> --timing   # full size: encode times at the defaults
//!
//! Each image is prepared as an export would be (8-bit, flattened, `Strip metadata` on),
//! encoded, decoded back **the way the viewer decodes it**, and both sides taken to linear
//! sRGB through their colour description before scoring — Image I/O hands AVIF and HEIC back
//! in Display P3, the others in the file's own space. Copies of one photo in several formats
//! (`x.jpg`, `x.webp`, `x - Copy.jpg`) count once, so they don't weigh the medians.
//!
//! Dev tool only: prints file names to the terminal, never writes anything.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use pb_encode::{Caps, ExportFormat, ExportImage, OutputColor, PrepareSpec, Resize, Samples};

type Encode = Box<dyn Fn(&ExportImage) -> Result<Vec<u8>, pb_encode::EncodeError>>;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(dir) = args.first().map(PathBuf::from) else {
        eprintln!("usage: codec_bench <folder> [--timing]");
        std::process::exit(2);
    };
    let timing = args.iter().any(|a| a == "--timing");
    let files = corpus(&dir);
    eprintln!("{} images", files.len());
    if timing {
        timing_pass(&files);
    } else {
        quality_pass(&files);
    }
}

/// The folder's decodable images, one per photo.
fn corpus(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("read the folder")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(pb_decode::is_supported_extension)
        })
        .collect();
    files.sort();
    let mut seen = HashSet::new();
    files.retain(|p| {
        let stem = p
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();
        let stem = stem.replace(" - copy", "").replace("-lossless", "");
        seen.insert(stem)
    });
    files
}

/// Decode and prepare one image as an 8-bit, opaque export, optionally down to `edge`.
fn prepared(path: &Path, edge: Option<u32>) -> Option<ExportImage> {
    let name = path.file_name()?.to_string_lossy().to_string();
    let bytes = std::fs::read(path).ok()?;
    let src = pb_decode::decode_named_bytes_for_export(&name, &bytes).ok()?;
    let spec = PrepareSpec {
        caps: Caps {
            alpha: false,
            sixteen_bit: false,
            gray: false,
        },
        resize: edge.map(Resize::LongEdge),
        ..PrepareSpec::full(0, true)
    };
    pb_encode::prepare(src, &spec).ok()
}

/// Encoded → linear sRGB through `t` (matrix + parametric curve, as the renderer does).
fn to_linear(px: &[u8], n: usize, t: &pb_decode::ColorTransform) -> Vec<[f32; 3]> {
    let srgb = |x: f32| {
        if x <= 0.04045 {
            x / 12.92
        } else {
            ((x + 0.055) / 1.055).powf(2.4)
        }
    };
    let curve = |x: f32| {
        let [g, a, b, c, d, e, f] = t.trc;
        if x < d {
            c * x + f
        } else {
            (a * x + b).max(0.0).powf(g) + e
        }
    };
    px.chunks_exact(n)
        .map(|p| {
            let v = [p[0], p[1], p[2]].map(|c| c as f32 / 255.0);
            if !t.enabled {
                return v.map(srgb);
            }
            let l = v.map(curve);
            let m = t.matrix;
            [0, 1, 2].map(|i| (m[i][0] * l[0] + m[i][1] * l[1] + m[i][2] * l[2]).max(0.0))
        })
        .collect()
}

fn reference(img: &ExportImage) -> Vec<[f32; 3]> {
    let Samples::U8(px) = &img.samples else {
        unreachable!("prepared to 8-bit")
    };
    let t = match &img.color {
        OutputColor::Srgb => pb_decode::ColorTransform::srgb(),
        OutputColor::Icc(p) => pb_decode::ColorTransform::from_icc(p),
    };
    to_linear(px, img.channels.count(), &t)
}

/// SSIMULACRA2 of `bytes` (decoded the way Export mode's preview reads it back) against the
/// reference. JPEG XL goes through libjxl, as the preview does: the viewer's jxl-oxide squeezes
/// wide-gamut colour into sRGB (see `export_preview::read_back_jxl`).
fn score(reference: &[[f32; 3]], w: u32, h: u32, bytes: &[u8], codec: &str) -> Option<f64> {
    let d = if codec == "JPEG XL" {
        let (width, height, rgba, icc) = pb_encode::jxl::decode_rgba8(bytes).ok()?;
        pb_decode::export::display_from_export_image(&ExportImage {
            width,
            height,
            channels: pb_encode::Channels::Rgba,
            samples: Samples::U8(rgba),
            color: if icc.is_empty() {
                OutputColor::Srgb
            } else {
                OutputColor::Icc(icc)
            },
            notes: Vec::new(),
            metadata: Default::default(),
        })
    } else {
        pb_decode::decode_bytes(bytes, None, false).ok()?
    };
    if d.width != w || d.height != h || d.format != pb_decode::PixelFormat::Rgba8 {
        return None;
    }
    let back = to_linear(&d.pixels, 4, &d.color);
    let a = ssimulacra2::LinearRgb::new(reference.to_vec(), w as usize, h as usize).ok()?;
    let b = ssimulacra2::LinearRgb::new(back, w as usize, h as usize).ok()?;
    ssimulacra2::compute_frame_ssimulacra2(a, b).ok()
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

fn pick(v: &mut [f64], q: f64) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[((v.len() - 1) as f64 * q).round() as usize]
}

/// The grid: (codec, quality, encoder).
fn grid() -> Vec<(&'static str, u8, Encode)> {
    let mut g: Vec<(&'static str, u8, Encode)> = Vec::new();
    let mut add = |name: &'static str, qs: &[u8], f: fn(u8) -> ExportFormat| {
        for &q in qs {
            let Ok(enc) = f(q).encoder() else {
                return; // not in this build
            };
            g.push((name, q, Box::new(move |i| enc.encode(i))));
        }
    };
    add("JPEG", &[70, 75, 80, 85, 90, 95], |quality| {
        ExportFormat::Jpeg { quality }
    });
    add("WebP", &[60, 65, 70, 75, 80, 85, 90, 95], |quality| {
        ExportFormat::Webp {
            quality,
            lossless: false,
        }
    });
    add(
        "AVIF",
        &[40, 50, 55, 60, 65, 70, 75, 80, 85, 90],
        |quality| ExportFormat::Avif { quality },
    );
    add("JPEG XL", &[60, 65, 70, 75, 80, 85, 90, 95], |quality| {
        ExportFormat::JpegXl {
            quality,
            lossless: false,
        }
    });
    add("HEIC", &[50, 55, 60, 65, 70, 75, 80, 85, 90], |quality| {
        ExportFormat::Heic { quality }
    });
    g
}

fn quality_pass(files: &[PathBuf]) {
    // (codec, quality) → (scores, bits per pixel, encode ms)
    type Acc = (Vec<f64>, Vec<f64>, Vec<f64>);
    let mut acc: BTreeMap<(&'static str, u8), Acc> = BTreeMap::new();
    // BENCH_CODECS="JPEG XL,HEIC" limits the grid; each image's scores go to stderr.
    let only = std::env::var("BENCH_CODECS").ok();
    let mut grid = grid();
    if let Some(only) = &only {
        grid.retain(|(c, _, _)| only.split(',').any(|o| o.trim() == *c));
    }
    for path in files {
        let Some(img) = prepared(path, Some(2048)) else {
            eprintln!("skip {}", path.display());
            continue;
        };
        let reference = reference(&img);
        let px = (img.width * img.height) as f64;
        eprint!("{:<48}", path.file_name().unwrap().to_string_lossy());
        for (codec, q, encode) in &grid {
            let t = Instant::now();
            let Ok(bytes) = encode(&img) else {
                eprint!(" {codec}{q}:enc-fail");
                continue;
            };
            let ms = t.elapsed().as_secs_f64() * 1e3;
            let Some(s) = score(&reference, img.width, img.height, &bytes, codec) else {
                eprint!(" {codec}{q}:dec-fail");
                continue;
            };
            eprint!(" {codec}{q}={s:.0}");
            let e = acc.entry((codec, *q)).or_default();
            e.0.push(s);
            e.1.push(bytes.len() as f64 * 8.0 / px);
            e.2.push(ms);
        }
        eprintln!();
    }

    println!(
        "\n{:<8} {:>4} {:>8} {:>8} {:>9} {:>8}",
        "codec", "q", "ssim2", "p10", "bits/px", "ms p50"
    );
    let mut by_codec: BTreeMap<&str, Vec<(u8, f64, f64)>> = BTreeMap::new();
    for ((codec, q), (mut s, mut b, mut t)) in acc {
        let (m, p10, bpp, ms) = (
            median(&mut s),
            pick(&mut s, 0.1),
            median(&mut b),
            median(&mut t),
        );
        println!("{codec:<8} {q:>4} {m:>8.2} {p10:>8.2} {bpp:>9.3} {ms:>8.0}");
        by_codec.entry(codec).or_default().push((q, m, bpp));
    }
    // Each codec's setting that matches JPEG 85's median score, and its size against JPEG's.
    let Some(&(_, target, jpeg_bpp)) = by_codec
        .get("JPEG")
        .and_then(|v| v.iter().find(|(q, _, _)| *q == 85))
    else {
        return;
    };
    println!("\nJPEG 85: median SSIMULACRA2 {target:.2} at {jpeg_bpp:.3} bits/px. Closest match per codec:");
    for (codec, rows) in &by_codec {
        if let Some(&(q, s, bpp)) = rows
            .iter()
            .min_by(|a, b| (a.1 - target).abs().total_cmp(&(b.1 - target).abs()))
        {
            println!(
                "  {codec:<8} q{q:<3} score {s:>6.2}  {bpp:.3} bits/px  ({:+.0}% vs JPEG 85)",
                (bpp / jpeg_bpp - 1.0) * 100.0
            );
        }
    }
}

fn timing_pass(files: &[PathBuf]) {
    use pb_app_core::export_panel::{
        AVIF_QUALITY_DEFAULT as AQ, HEIC_QUALITY_DEFAULT as HQ, JPEG_QUALITY_DEFAULT as JQ,
        JXL_QUALITY_DEFAULT as XQ, WEBP_QUALITY_DEFAULT as WQ,
    };
    let mut cases: Vec<(String, Encode)> = vec![
        (
            "JPEG".into(),
            Box::new(move |i| pb_encode::encode_jpeg(i, JQ)),
        ),
        (
            "WebP".into(),
            Box::new(move |i| pb_encode::webp::encode_webp(i, WQ, false)),
        ),
    ];
    for effort in [0u8, 10, 25, 50, 75] {
        cases.push((
            format!("WebP lossless e{effort}"),
            Box::new(move |i| pb_encode::webp::encode_webp_at(i, WQ, true, effort)),
        ));
    }
    for speed in [5u8, 6, 7, 8] {
        cases.push((
            format!("AVIF speed {speed}"),
            Box::new(move |i| pb_encode::avif::encode_avif_at(i, AQ, speed)),
        ));
    }
    for effort in [5i64, 6, 7] {
        cases.push((
            format!("JPEG XL effort {effort}"),
            Box::new(move |i| pb_encode::jxl::encode_jxl_at(i, XQ, false, effort)),
        ));
    }
    cases.push((
        "JPEG XL lossless".into(),
        Box::new(move |i| pb_encode::jxl::encode_jxl(i, XQ, true)),
    ));
    #[cfg(target_os = "macos")]
    cases.push((
        "HEIC".into(),
        Box::new(move |i| pb_encode::heic::encode_heic(i, HQ)),
    ));
    let _ = HQ;
    cases.push(("TIFF".into(), Box::new(pb_encode::tiff::encode_tiff)));

    let mut times: Vec<Vec<f64>> = vec![Vec::new(); cases.len()];
    for path in files {
        let Some(img) = prepared(path, None) else {
            continue;
        };
        let mp = (img.width * img.height) as f64 / 1e6;
        eprint!(
            "{:<44} {:>5.1} MP",
            path.file_name().unwrap().to_string_lossy(),
            mp
        );
        for (i, (name, encode)) in cases.iter().enumerate() {
            let t = Instant::now();
            if let Ok(bytes) = encode(&img) {
                let ms = t.elapsed().as_secs_f64() * 1e3;
                times[i].push(ms);
                let bpp = bytes.len() as f64 * 8.0 / (mp * 1e6);
                eprint!(" | {name} {ms:.0} ms {bpp:.2} bpp");
            }
        }
        eprintln!();
    }
    println!(
        "\n{:<22} {:>8} {:>8} {:>8}",
        "encode (full size)", "p50 ms", "p95 ms", "max ms"
    );
    for (i, (name, _)) in cases.iter().enumerate() {
        let t = &mut times[i];
        if t.is_empty() {
            continue;
        }
        println!(
            "{name:<22} {:>8.0} {:>8.0} {:>8.0}",
            pick(t, 0.5),
            pick(t, 0.95),
            pick(t, 1.0)
        );
    }
}
