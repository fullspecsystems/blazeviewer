//! Decode-throughput spike.
//!
//! Answers the load-bearing question behind the native-D3D12 / GPU-decode bet:
//! **can a CPU decode pool sustain 120 Hz on fit-sized frames?**
//!
//! It benchmarks the pure-Rust decode path (zune-jpeg full decode + jpeg-decoder
//! DCT scaled-to-fit) over a real photo corpus, single-threaded (per-image
//! latency) and across all cores (pool throughput). Pure Rust = no NASM/cmake, so
//! it's a conservative lower bound; turbojpeg would be faster and scale finer.
//!
//! Usage: `decode-throughput-spike [DIR] [SAMPLE_CAP]`  (defaults: D:\Pictures, 200)

use std::io::Cursor;
use std::path::PathBuf;
use std::time::Instant;

use rayon::prelude::*;

// The display we're fitting to (7680x3840, the owner's double-width 4K ultrawide).
const SCREEN_W: u32 = 7680;
const SCREEN_H: u32 = 3840;
const REFRESH_HZ: f64 = 120.0;

struct Sample {
    bytes: Vec<u8>,
}

struct Row {
    mp: f64,
    file_kb: f64,
    full_ms: f64,
    scaled_ms: f64,
    downscaled: bool,
}

/// Full-resolution decode with zune-jpeg. Returns (w, h, pixel_bytes).
fn decode_full(bytes: &[u8]) -> Option<(u32, u32, usize)> {
    let mut d = zune_jpeg::JpegDecoder::new(bytes);
    let px = d.decode().ok()?;
    let (w, h) = d.dimensions()?;
    Some((w as u32, h as u32, px.len()))
}

/// Read just the JPEG dimensions (cheap header parse) via jpeg-decoder.
fn dims(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut d = jpeg_decoder::Decoder::new(Cursor::new(bytes));
    d.read_info().ok()?;
    let info = d.info()?;
    Some((info.width as u32, info.height as u32))
}

/// Largest "contain" fit inside the screen without upscaling.
fn fit_dims(w: u32, h: u32, sw: u32, sh: u32) -> (u32, u32) {
    let s = (sw as f64 / w as f64).min(sh as f64 / h as f64).min(1.0);
    let fw = ((w as f64 * s).round() as u32).clamp(1, 65535);
    let fh = ((h as f64 * s).round() as u32).clamp(1, 65535);
    (fw, fh)
}

/// Decode-to-fit: request the on-screen size; jpeg-decoder picks 1, 1/2, 1/4, 1/8.
/// Returns (full_w, full_h, out_w, out_h).
fn decode_scaled_to_fit(bytes: &[u8], sw: u32, sh: u32) -> Option<(u32, u32, u32, u32)> {
    let (fw, fh) = dims(bytes)?;
    let (tw, th) = fit_dims(fw, fh, sw, sh);
    let mut d = jpeg_decoder::Decoder::new(Cursor::new(bytes));
    let (ow, oh) = d.scale(tw as u16, th as u16).ok()?;
    let _ = d.decode().ok()?;
    Some((fw, fh, ow as u32, oh as u32))
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().unwrap_or_else(|| r"D:\Pictures".to_string());
    let cap: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(200);

    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0);

    eprintln!("scanning {dir} for JPEGs ...");
    let mut paths: Vec<PathBuf> = walkdir::WalkDir::new(&dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .map(|x| {
                    let x = x.to_ascii_lowercase();
                    x == "jpg" || x == "jpeg"
                })
                .unwrap_or(false)
        })
        .map(|e| e.into_path())
        .collect();

    let total = paths.len();
    if total == 0 {
        eprintln!("no JPEGs found under {dir}; nothing to benchmark.");
        return;
    }
    paths.sort();
    // Even spread across the (sorted) set so we sample large and small alike.
    if paths.len() > cap {
        let step = paths.len() as f64 / cap as f64;
        paths = (0..cap).map(|i| paths[(i as f64 * step) as usize].clone()).collect();
    }
    eprintln!("found {total} JPEGs; sampling {} for the benchmark", paths.len());

    // Load bytes up front so disk I/O never pollutes decode timing.
    let corpus: Vec<Sample> = paths
        .iter()
        .filter_map(|p| std::fs::read(p).ok().map(|bytes| Sample { bytes }))
        .collect();
    eprintln!("loaded {} files into RAM; decoding ...", corpus.len());

    // --- single-thread per-image latency ---
    let mut rows: Vec<Row> = Vec::new();
    for s in &corpus {
        let t = Instant::now();
        let full = decode_full(&s.bytes);
        let full_ms = t.elapsed().as_secs_f64() * 1e3;
        let (fw, fh, _) = match full {
            Some(v) => v,
            None => continue,
        };

        let t = Instant::now();
        let scaled = decode_scaled_to_fit(&s.bytes, SCREEN_W, SCREEN_H);
        let scaled_ms = t.elapsed().as_secs_f64() * 1e3;
        let downscaled = matches!(scaled, Some((_, _, ow, oh)) if ow < fw || oh < fh);

        rows.push(Row {
            mp: (fw as f64 * fh as f64) / 1e6,
            file_kb: s.bytes.len() as f64 / 1024.0,
            full_ms,
            scaled_ms,
            downscaled,
        });
    }

    // --- all-core pool throughput ---
    let t = Instant::now();
    let n_full: usize = corpus.par_iter().filter_map(|s| decode_full(&s.bytes)).count();
    let full_wall = t.elapsed().as_secs_f64();

    let t = Instant::now();
    let n_scaled: usize = corpus
        .par_iter()
        .filter_map(|s| decode_scaled_to_fit(&s.bytes, SCREEN_W, SCREEN_H))
        .count();
    let scaled_wall = t.elapsed().as_secs_f64();

    report(cores, total, &rows, n_full, full_wall, n_scaled, scaled_wall);
}

fn pctl(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = (((sorted.len() - 1) as f64) * p).round() as usize;
    sorted[idx]
}

fn report(
    cores: usize,
    total: usize,
    rows: &[Row],
    n_full: usize,
    full_wall: f64,
    n_scaled: usize,
    scaled_wall: f64,
) {
    let mut full_ms: Vec<f64> = rows.iter().map(|r| r.full_ms).collect();
    let mut scaled_ms: Vec<f64> = rows.iter().map(|r| r.scaled_ms).collect();
    full_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    scaled_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let total_mp: f64 = rows.iter().map(|r| r.mp).sum();
    let mp_lo = rows.iter().map(|r| r.mp).fold(f64::INFINITY, f64::min);
    let mp_hi = rows.iter().map(|r| r.mp).fold(0.0_f64, f64::max);
    let kb_hi = rows.iter().map(|r| r.file_kb).fold(0.0_f64, f64::max);
    let downscaled = rows.iter().filter(|r| r.downscaled).count();

    let full_ips = n_full as f64 / full_wall;
    let scaled_ips = n_scaled as f64 / scaled_wall;
    let full_mps = total_mp / full_wall;
    let budget = REFRESH_HZ;

    // buckets
    let buckets = [
        ("< 4 MP", 0.0, 4.0),
        ("4-12 MP", 4.0, 12.0),
        ("12-24 MP", 12.0, 24.0),
        ("24-50 MP", 24.0, 50.0),
        ("> 50 MP", 50.0, f64::INFINITY),
    ];

    let mut md = String::new();
    md.push_str("# Decode-throughput spike — results\n\n");
    md.push_str(&format!(
        "Pure-Rust decode path (zune-jpeg full / jpeg-decoder scaled-to-fit) over a \
         real photo corpus. Conservative lower bound — turbojpeg would be faster and \
         scale finer (1/8 steps).\n\n"
    ));
    md.push_str(&format!("- **Cores:** {cores}\n"));
    md.push_str(&format!(
        "- **Corpus:** {} sampled of {} JPEGs · {:.1}–{:.1} MP · up to {:.1} MB/file\n",
        rows.len(),
        total,
        mp_lo,
        mp_hi,
        kb_hi / 1024.0
    ));
    md.push_str(&format!("- **Fit target:** {SCREEN_W}×{SCREEN_H} @ {REFRESH_HZ} Hz (budget {:.2} ms/frame)\n\n", 1000.0 / REFRESH_HZ));

    md.push_str("## Headline — all-core pool throughput\n\n");
    md.push_str("| Path | images/s | vs 120 Hz | MP/s | wall |\n|---|---:|---:|---:|---:|\n");
    md.push_str(&format!(
        "| Full decode (zune) | **{:.0}** | **{:.1}×** | {:.0} | {:.2}s |\n",
        full_ips,
        full_ips / budget,
        full_mps,
        full_wall
    ));
    md.push_str(&format!(
        "| Scaled-to-fit (jpeg-decoder) | **{:.0}** | **{:.1}×** | — | {:.2}s |\n\n",
        scaled_ips,
        scaled_ips / budget,
        scaled_wall
    ));

    md.push_str("## Single-thread per-image latency\n\n");
    md.push_str("| Path | p50 | p95 | max |\n|---|---:|---:|---:|\n");
    md.push_str(&format!(
        "| Full decode | {:.1} ms | {:.1} ms | {:.1} ms |\n",
        pctl(&full_ms, 0.50),
        pctl(&full_ms, 0.95),
        full_ms.last().copied().unwrap_or(0.0)
    ));
    md.push_str(&format!(
        "| Scaled-to-fit | {:.1} ms | {:.1} ms | {:.1} ms |\n\n",
        pctl(&scaled_ms, 0.50),
        pctl(&scaled_ms, 0.95),
        scaled_ms.last().copied().unwrap_or(0.0)
    ));

    md.push_str(&format!(
        "**Decode-to-fit triggered on {}/{} images** ({:.0}%). On a {SCREEN_W}-wide \
         screen, power-of-2 DCT scaling rarely fires (halving undershoots the large \
         on-screen size), so jpeg-decoder's scaled path ≈ full decode. turbojpeg's \
         1/8-granular scaling would help more here.\n\n",
        downscaled,
        rows.len(),
        100.0 * downscaled as f64 / rows.len().max(1) as f64
    ));

    md.push_str("## By megapixel bucket (single-thread median)\n\n");
    md.push_str("| Bucket | n | full p50 | scaled p50 | downscaled |\n|---|---:|---:|---:|---:|\n");
    for (name, lo, hi) in buckets {
        let mut f: Vec<f64> = rows.iter().filter(|r| r.mp >= lo && r.mp < hi).map(|r| r.full_ms).collect();
        let mut s: Vec<f64> = rows.iter().filter(|r| r.mp >= lo && r.mp < hi).map(|r| r.scaled_ms).collect();
        let n = f.len();
        if n == 0 {
            continue;
        }
        f.sort_by(|a, b| a.partial_cmp(b).unwrap());
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let ds = rows.iter().filter(|r| r.mp >= lo && r.mp < hi && r.downscaled).count();
        md.push_str(&format!(
            "| {name} | {n} | {:.1} ms | {:.1} ms | {ds} |\n",
            pctl(&f, 0.50),
            pctl(&s, 0.50)
        ));
    }

    // verdict
    let verdict = if full_ips >= budget {
        format!(
            "\n## Verdict\n\nThe **CPU full-decode path alone sustains {:.0} img/s — {:.1}× the \
             120 Hz budget** on {cores} cores. Decode-to-fit is barely needed on this screen. \
             This means the prefetch ring can stay warm at refresh rate *without* GPU decode, \
             which weakens the case for the native-D3D12 / nvImageCodec / zero-copy complexity. \
             Hand these numbers to the review.\n",
            full_ips,
            full_ips / budget
        )
    } else {
        format!(
            "\n## Verdict\n\nThe CPU full-decode path sustains {:.0} img/s ({:.1}× the 120 Hz \
             budget) on {cores} cores — below a comfortable margin. GPU decode / zero-copy may \
             be justified; turbojpeg + decode-to-fit should be measured before deciding.\n",
            full_ips,
            full_ips / budget
        )
    };
    md.push_str(&verdict);

    // write + print
    let out = std::path::Path::new(".taskmaster/reports");
    let _ = std::fs::create_dir_all(out);
    let path = out.join("decode-spike.md");
    if std::fs::write(&path, &md).is_ok() {
        eprintln!("wrote {}", path.display());
    }
    println!("\n{md}");
}
