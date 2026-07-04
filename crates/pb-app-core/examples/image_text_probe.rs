//! Dev probe for the "text in image" scan (task #45) — the `archive_probe`
//! precedent: run the exact worker job the app runs (full-res decode → QR decode →
//! on-device OCR) against one file and print what it found. Exercises the real OS
//! engine (Windows.Media.Ocr here), which the unit tests deliberately can't.
//!
//! ```sh
//! cargo run -p pb-app-core --example image_text_probe -- path/to/photo.png
//! ```

use std::time::Instant;

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: image_text_probe <image>");
        std::process::exit(2);
    };
    let source = pb_source::FsSource::new(vec![path.clone().into()]);
    let t0 = Instant::now();
    let r = pb_app_core::image_text::scan_job(&source, 0, pb_render::Rotation::default());
    println!("{path}: scanned in {:?}", t0.elapsed());
    for q in &r.qr {
        println!("QR: {q}");
    }
    for l in &r.lines {
        println!("| {l}");
    }
    if let Some(e) = &r.ocr_error {
        println!("OCR error: {e}");
    }
    if r.is_empty() && r.ocr_error.is_none() {
        println!("(no text found)");
    }
}
