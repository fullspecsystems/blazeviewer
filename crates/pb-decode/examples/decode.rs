//! A tiny decode CLI for verifying backends against a corpus:
//!
//!   cargo run -q --example decode -p pb-decode -- <file> [<file> ...]
//!
//! Prints one line per file: OK with dimensions/codec/byte-count, or FAIL with
//! the error. Decodes at full resolution (no fit box). Handy for smoke-testing
//! new formats without launching the GUI.

use std::path::Path;

use pb_decode::decode_image_file;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: decode <file> [<file> ...]");
        return;
    }
    let mut ok = 0;
    let mut fail = 0;
    for arg in &args {
        match decode_image_file(Path::new(arg), None, false) {
            Ok(img) => {
                ok += 1;
                let color = if img.color.enabled {
                    format!(
                        "CMS[g={:.3} m00={:.3} m10={:.3} m20={:.3}]",
                        img.color.trc[0],
                        img.color.matrix[0][0],
                        img.color.matrix[1][0],
                        img.color.matrix[2][0],
                    )
                } else {
                    "sRGB".to_string()
                };
                println!(
                    "OK    {:<8} {:>5}x{:<5} {:>11} px-bytes  {:<28} {}",
                    img.codec,
                    img.orig_width,
                    img.orig_height,
                    img.pixels.len(),
                    color,
                    arg
                );
            }
            Err(e) => {
                fail += 1;
                println!("FAIL  {:<8} {:<33} {}", "-", e.to_string(), arg);
            }
        }
    }
    println!("\n{ok} ok, {fail} fail, {} total", args.len());
}
