//! Dev spike (task #38): decode a Live Photo motion `.mov` via AVFoundation and dump
//! stats, writing a couple of frames to PNG so we can eyeball rotation + pixels.
//!
//!   cargo run -p pb-decode --example live_probe -- <file.mov>... [--dump <dir>]
//!
//! macOS-only (AVFoundation). Pass `--dump <dir>` to write frame 0 + the middle frame
//! of each clip as PNGs into <dir>.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("live_probe is macOS-only (AVFoundation).");
}

#[cfg(target_os = "macos")]
fn main() {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    let mut files: Vec<String> = Vec::new();
    let mut dump: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--dump" {
            dump = args.next().map(PathBuf::from);
        } else {
            files.push(a);
        }
    }
    if files.is_empty() {
        eprintln!("usage: live_probe <file.mov>... [--dump <dir>]");
        std::process::exit(2);
    }
    if let Some(dir) = &dump {
        let _ = std::fs::create_dir_all(dir);
    }

    // Cap the long edge like the app would (decode-to-fit → bounds RAM).
    const MAX_LONG_EDGE: u32 = 1440;

    for path in &files {
        let t0 = Instant::now();
        match pb_decode::decode_live_motion(Path::new(path), MAX_LONG_EDGE) {
            Ok(anim) => {
                let elapsed = t0.elapsed();
                let total: Duration = anim.frames.iter().map(|f| f.delay).sum();
                let fps = if total.as_secs_f64() > 0.0 {
                    anim.frames.len() as f64 / total.as_secs_f64()
                } else {
                    0.0
                };
                let bytes: usize = anim.frames.iter().map(|f| f.rgba.len()).sum();
                println!(
                    "{path}: {}x{} frames={} ~{:.1}fps dur={:.2}s loop={} decode={:.0}ms ({:.1}ms/frame) ram={:.0}MB{}",
                    anim.width,
                    anim.height,
                    anim.frames.len(),
                    fps,
                    total.as_secs_f64(),
                    anim.loop_count,
                    elapsed.as_secs_f64() * 1000.0,
                    elapsed.as_secs_f64() * 1000.0 / anim.frames.len().max(1) as f64,
                    bytes as f64 / 1_048_576.0,
                    if anim.truncated { " (TRUNCATED)" } else { "" },
                );
                if let Some(dir) = &dump {
                    let stem = Path::new(path)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "clip".into());
                    for (label, idx) in [("first", 0), ("mid", anim.frames.len() / 2)] {
                        let f = &anim.frames[idx];
                        let out = dir.join(format!("{stem}_{label}.png"));
                        match image::save_buffer(
                            &out,
                            &f.rgba,
                            f.width,
                            f.height,
                            image::ColorType::Rgba8,
                        ) {
                            Ok(()) => println!("    wrote {}", out.display()),
                            Err(e) => println!("    PNG write failed: {e}"),
                        }
                    }
                }
            }
            Err(e) => println!("{path}: decode failed: {e}"),
        }
    }
}
