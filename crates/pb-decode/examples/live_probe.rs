//! Dev harness (tasks #38 / #39 / #69): decode a Live Photo motion `.mov` (AVFoundation
//! on macOS, Media Foundation on Windows) and dump stats, writing a couple of frames to
//! PNG so we can eyeball rotation + pixels.
//!
//!   cargo run --release -p pb-decode --example live_probe -- <file.mov>... [--dump <dir>]
//!
//! Pass `--dump <dir>` to write frame 0 + the middle frame of each clip as PNGs
//! into <dir>.
//!
//! On macOS each clip is measured through the **streaming** decoder first (task #69's
//! whole point is time-to-first-frame — the batch line can't show it), then through the
//! batch wrapper (whose collector also validates the chunk contract), plus a
//! cancellation-latency run (cancel set at the first frame → how fast the worker stops).

#[cfg(not(any(target_os = "macos", windows)))]
fn main() {
    eprintln!("live_probe needs an OS motion decoder (macOS AVFoundation / Windows MF).");
}

#[cfg(any(target_os = "macos", windows))]
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
        // Streaming stats (macOS AVAssetReader, task #69): what the chunk callback can
        // observe — time-to-header (≈ setup), time-to-first-frame (the user-facing win),
        // total, and how fast a cancel stops the worker.
        #[cfg(target_os = "macos")]
        {
            use std::sync::atomic::{AtomicBool, Ordering};

            let t0 = Instant::now();
            let (mut t_header, mut t_first, mut frames, mut dims) =
                (None::<Duration>, None::<Duration>, 0usize, (0u32, 0u32));
            let mut failed = None::<String>;
            pb_decode::decode_live_motion_streaming(
                Path::new(path),
                MAX_LONG_EDGE,
                &AtomicBool::new(false),
                &mut |chunk| match chunk {
                    pb_decode::MotionChunk::Header(h) => {
                        t_header.get_or_insert(t0.elapsed());
                        dims = (h.width, h.height);
                    }
                    pb_decode::MotionChunk::Frame(_) => {
                        t_first.get_or_insert(t0.elapsed());
                        frames += 1;
                    }
                    pb_decode::MotionChunk::Done { .. } => {}
                    pb_decode::MotionChunk::Failed(e) => failed = Some(e.to_string()),
                },
            );
            let total = t0.elapsed();
            match failed {
                Some(e) => println!("{path} [stream]: FAILED: {e}"),
                None => {
                    // Cancel latency: a second stream, cancelled at its first frame — the
                    // gap from `store` to the decoder returning is what navigation costs.
                    let cancel = AtomicBool::new(false);
                    let mut t_cancel = None::<Instant>;
                    pb_decode::decode_live_motion_streaming(
                        Path::new(path),
                        MAX_LONG_EDGE,
                        &cancel,
                        &mut |chunk| {
                            if matches!(chunk, pb_decode::MotionChunk::Frame(_))
                                && t_cancel.is_none()
                            {
                                cancel.store(true, Ordering::Relaxed);
                                t_cancel = Some(Instant::now());
                            }
                        },
                    );
                    let cancel_ms = t_cancel
                        .map(|t| t.elapsed().as_secs_f64() * 1000.0)
                        .unwrap_or(f64::NAN);
                    println!(
                        "{path} [stream]: {}x{} header={:.0}ms first_frame={:.0}ms total={:.0}ms frames={} ({:.1}ms/frame) cancel={cancel_ms:.0}ms",
                        dims.0,
                        dims.1,
                        t_header.unwrap_or_default().as_secs_f64() * 1000.0,
                        t_first.unwrap_or_default().as_secs_f64() * 1000.0,
                        total.as_secs_f64() * 1000.0,
                        frames,
                        total.as_secs_f64() * 1000.0 / frames.max(1) as f64,
                    );
                }
            }
        }

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
