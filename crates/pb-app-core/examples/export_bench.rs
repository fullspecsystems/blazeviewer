//! Export pipeline sweep + PNG effort benchmark (task #89, export-plan.md rung 1: "pick the
//! PNG default from corpus numbers — encode p50/p95 and bytes per megapixel").
//!
//! For every image in a folder: the export decode → `prepare` → PNG at each effort, then a
//! decode-back check (dimensions survive, the file is valid). Prints one line per file and a
//! p50/p95 summary per effort. Read-only on the input folder; PNGs go to `--out` if given.
//!
//!     cargo run --release -p pb-app-core --example export_bench -- <folder> [--out <dir>]
//!
//! Dev tool only — prints file names to the terminal, never persists anything.

use std::path::{Path, PathBuf};
use std::time::Instant;

use pb_encode::{encode_png, prepare, PngEffort, PrepareSpec};

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let i = ((v.len() as f64 - 1.0) * p).round() as usize;
    v[i]
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(dir) = args.first().map(PathBuf::from) else {
        eprintln!("usage: export_bench <folder> [--out <dir>]");
        std::process::exit(2);
    };
    let out = args
        .iter()
        .position(|a| a == "--out")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);
    if let Some(o) = &out {
        std::fs::create_dir_all(o).expect("create --out");
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read folder")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();
    files.sort();

    let efforts = [PngEffort::Fast, PngEffort::Balanced, PngEffort::High];
    let mut times: Vec<Vec<f64>> = vec![Vec::new(); 3];
    let mut bpm: Vec<Vec<f64>> = vec![Vec::new(); 3];
    let mut decode_ms = Vec::new();
    let (mut ok, mut refused, mut failed) = (0, 0, 0);

    for path in &files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let bytes = std::fs::read(path).expect("read");
        let t = Instant::now();
        let src = match pb_decode::decode_named_bytes_for_export(&name, &bytes) {
            Ok(s) => s,
            Err(pb_decode::ExportDecodeError::Refused(why)) => {
                println!("REFUSED  {name}: {why}");
                refused += 1;
                continue;
            }
            Err(e) => {
                println!("SKIP     {name}: {e}");
                failed += 1;
                continue;
            }
        };
        let dt = t.elapsed().as_secs_f64() * 1e3;
        decode_ms.push(dt);
        let (w, h, ch, depth, color, path_used, hdr) = (
            src.width,
            src.height,
            src.channels,
            src.samples.bit_depth(),
            match &src.color {
                pb_encode::ColorDesc::Icc(b) => {
                    format!(
                        "icc[{}]",
                        pb_encode::icc::description(b).unwrap_or_default()
                    )
                }
                other => format!("{other:?}"),
            },
            src.provenance.path,
            src.is_hdr(),
        );
        let img = match prepare(
            src,
            &PrepareSpec {
                rotation_cw: 0,
                strip_metadata: true,
            },
        ) {
            Ok(i) => i,
            Err(e) => {
                println!("FAIL     {name}: prepare: {e}");
                failed += 1;
                continue;
            }
        };
        let mp = (img.width as f64 * img.height as f64) / 1e6;
        let mut line = format!(
            "OK       {name}: {w}x{h} {ch:?} {depth}-bit {color} via {path_used}{} | decode {dt:.0}ms |",
            if hdr { " HDR" } else { "" }
        );
        for (i, effort) in efforts.iter().enumerate() {
            let t = Instant::now();
            let png = encode_png(&img, *effort).expect("encode");
            let et = t.elapsed().as_secs_f64() * 1e3;
            times[i].push(et);
            bpm[i].push(png.len() as f64 / mp);
            line.push_str(&format!(
                " {effort:?} {et:.0}ms {:.1}MB",
                png.len() as f64 / 1e6
            ));
            if *effort == PngEffort::Balanced {
                let back =
                    pb_decode::decode_named_bytes("x.png", &png, None, false).expect("decode back");
                assert_eq!(
                    (back.width, back.height),
                    (img.width, img.height),
                    "{name}: dims survive"
                );
                if let Some(o) = &out {
                    let stem = Path::new(&name)
                        .file_stem()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned();
                    std::fs::write(o.join(format!("{stem}.png")), &png).expect("write");
                }
            }
        }
        if !img.notes.is_empty() {
            line.push_str(&format!(" notes {:?}", img.notes));
        }
        println!("{line}");
        ok += 1;
    }

    println!(
        "\n{ok} exported, {refused} refused, {failed} skipped/failed of {}",
        files.len()
    );
    println!(
        "decode p50 {:.0}ms p95 {:.0}ms",
        pct(&mut decode_ms.clone(), 0.5),
        pct(&mut decode_ms, 0.95)
    );
    for (i, effort) in efforts.iter().enumerate() {
        println!(
            "{effort:?}: encode p50 {:.0}ms p95 {:.0}ms | bytes/MP p50 {:.2}MB",
            pct(&mut times[i].clone(), 0.5),
            pct(&mut times[i], 0.95),
            pct(&mut bpm[i], 0.5) / 1e6
        );
    }
}
