//! Dev tool: dump animation stats for files passed on the command line.
//!
//!   cargo run -p pb-decode --example anim_info -- <file>...
//!
//! Prints whether each file is detected as animated, and (if so) its kind, frame
//! count, loop count, canvas size, and the distinct per-frame delays. Handy for
//! eyeballing a corpus while developing the animated-image feature.

use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: anim_info <file>...");
        std::process::exit(2);
    }
    for path in args {
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                println!("{path}: read error: {e}");
                continue;
            }
        };
        match pb_decode::detect_animation(&bytes) {
            None => println!("{path}: still (not animated)"),
            Some(kind) => {
                let t0 = Instant::now();
                let decoded = pb_decode::decode_animation(&bytes, None);
                let elapsed = t0.elapsed();
                match decoded {
                    Ok(a) => {
                        let loops = if a.loop_count == 0 {
                            "infinite".to_string()
                        } else {
                            a.loop_count.to_string()
                        };
                        let total: Duration = a.frames.iter().map(|f| f.delay).sum();
                        println!(
                            "{path}: {:?} {}x{} frames={} loop={} total={:.2}s{}",
                            kind,
                            a.width,
                            a.height,
                            a.frame_count(),
                            loops,
                            total.as_secs_f64(),
                            if a.truncated { " (TRUNCATED)" } else { "" },
                        );
                        let mut delays: Vec<u128> =
                            a.frames.iter().map(|f| f.delay.as_millis()).collect();
                        delays.sort_unstable();
                        delays.dedup();
                        println!("    distinct frame delays (ms): {delays:?}");
                        println!(
                            "    decode_animation: {:.0}ms ({:.1}ms/frame)",
                            elapsed.as_secs_f64() * 1000.0,
                            elapsed.as_secs_f64() * 1000.0 / a.frame_count().max(1) as f64,
                        );
                    }
                    Err(e) => println!("{path}: detected {kind:?} but decode failed: {e}"),
                }
            }
        }
    }
}
