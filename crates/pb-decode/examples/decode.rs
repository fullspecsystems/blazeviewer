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
        match decode_image_file(Path::new(arg), None) {
            Ok(img) => {
                ok += 1;
                println!(
                    "OK    {:<8} {:>5}x{:<5} {:>11} px-bytes  {}",
                    img.codec,
                    img.orig_width,
                    img.orig_height,
                    img.pixels.len(),
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
