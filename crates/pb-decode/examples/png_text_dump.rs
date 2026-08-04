//! Dump the generation-metadata text chunks of a PNG (task #137).
//!
//! A dev tool for checking the extractor against real files, which the test
//! suite cannot do — the corpus is not committed (the plan's *Fixtures* note).
//!
//! ```sh
//! cargo run -p pb-decode --example png_text_dump -- path\to\image.png
//! ```

fn main() {
    let mut any = false;
    for path in std::env::args().skip(1) {
        any = true;
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("{path}: cannot read");
            continue;
        };
        let chunks = pb_decode::read_png_text(&bytes);
        println!("{path}  ({} bytes)", bytes.len());
        if chunks.is_empty() {
            println!("  (no generation metadata)");
        }
        for (keyword, text) in &chunks {
            println!("  [{keyword}] {} bytes", text.len());
            let preview: String = text.chars().take(160).collect();
            println!("      {preview}…");
        }
    }
    if !any {
        eprintln!("usage: png_text_dump <file.png> [more.png ...]");
    }
}
