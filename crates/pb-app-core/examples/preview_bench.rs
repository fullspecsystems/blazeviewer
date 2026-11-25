//! Export mode's live preview, timed (task #89 rung 3): how long after a setting settles
//! until the preview — and with it the exact file size — is ready. Runs the real preview
//! worker over every image in a folder, the way the panel drives it: the first preview of
//! an item (decode + the original side + the export), then a setting change on the same item
//! (the export side only — the decoded source is cached). Prints one line per file and a
//! p50/p95 per case.
//!
//!     cargo run --release -p pb-app-core --example preview_bench -- <folder>
//!
//! Dev tool only — prints file names to the terminal, never writes anything.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pb_app_core::export_panel::{ExportSettings, FormatChoice, PngCompression, SizeChoice};
use pb_app_core::export_preview::{PreviewKey, PreviewRequest, PreviewWorker, SourceKey};
use pb_app_core::frame_capture::FrameKey;
use pb_source::ItemSource;

fn main() {
    let Some(dir) = std::env::args().nth(1).map(PathBuf::from) else {
        eprintln!("usage: preview_bench <folder>");
        std::process::exit(2);
    };
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read the folder")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(pb_decode::is_supported_extension)
        })
        .collect();
    files.sort();
    let source: Arc<dyn ItemSource> = Arc::new(pb_source::FsSource::new(files.clone()));

    let cases: [(&str, ExportSettings); 4] = [
        (
            "PNG Balanced full",
            settings(FormatChoice::Png, 85, SizeChoice::Full),
        ),
        (
            "JPEG 85 full",
            settings(FormatChoice::Jpeg, 85, SizeChoice::Full),
        ),
        (
            "JPEG 70 full",
            settings(FormatChoice::Jpeg, 70, SizeChoice::Full),
        ),
        (
            "JPEG 85 2048",
            settings(FormatChoice::Jpeg, 85, SizeChoice::LongEdge(2048)),
        ),
    ];
    let mut times: Vec<Vec<Duration>> = vec![Vec::new(); cases.len()];
    let worker = PreviewWorker::new();
    let mut gen = 0;
    for (item, path) in files.iter().enumerate() {
        let name = source.name(item).to_string();
        let mut line = format!(
            "{:<48}",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
        for (i, (label, s)) in cases.iter().enumerate() {
            gen += 1;
            let key = PreviewKey {
                source: SourceKey {
                    path: Some(path.clone()),
                    container: None,
                    name: name.clone(),
                    frame: FrameKey::Still,
                    content_gen: 0,
                },
                rotation_cw: 0,
                settings: *s,
            };
            let start = Instant::now();
            worker.submit(PreviewRequest {
                gen,
                key,
                source: Arc::clone(&source),
                item,
                frame: None,
                need_original: i == 0, // the first preview of an item makes both sides
                display_turns: 0,
                queued_at: start,
            });
            let r = loop {
                if let Some(r) = worker.drain().expect("the preview worker died").pop() {
                    break r;
                }
                std::thread::sleep(Duration::from_millis(1));
            };
            let took = start.elapsed();
            match r.export {
                Ok(e) => {
                    times[i].push(took);
                    line += &format!(
                        " | {label} {:.0}ms {}",
                        took.as_secs_f64() * 1e3,
                        pb_app_core::export_preview::size_label(e.bytes.len() as u64)
                    );
                }
                Err(why) => {
                    line += &format!(" | {label} failed ({why})");
                    break;
                }
            }
        }
        println!("{line}");
    }
    println!();
    for (i, (label, _)) in cases.iter().enumerate() {
        let t = &mut times[i];
        t.sort();
        if t.is_empty() {
            continue;
        }
        let pick = |q: f64| t[((t.len() - 1) as f64 * q).round() as usize];
        println!(
            "{label:<20} {} previews: p50 {:.0}ms p95 {:.0}ms max {:.0}ms{}",
            t.len(),
            pick(0.5).as_secs_f64() * 1e3,
            pick(0.95).as_secs_f64() * 1e3,
            t.last().unwrap().as_secs_f64() * 1e3,
            if i == 0 {
                "  (first preview of an item: decode + both sides)"
            } else {
                "  (a setting change: export side only)"
            }
        );
    }
}

fn settings(format: FormatChoice, q: u8, size: SizeChoice) -> ExportSettings {
    ExportSettings {
        format,
        jpeg_quality: q,
        png: PngCompression::Balanced,
        size,
        strip_metadata: true,
    }
}
