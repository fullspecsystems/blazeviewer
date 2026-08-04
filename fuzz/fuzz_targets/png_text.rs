//! Fuzz the PNG generation-metadata text-chunk reader (task #137): arbitrary
//! bytes must never panic, overflow, or over-allocate — only an empty or
//! partial `Vec`.
//!
//! This target matters more than most: `read_png_text` is called **directly**
//! from the metadata path, so it does not sit under `pb-decode`'s private
//! `catch_panics` wrapper the image decoders enjoy. Its panic-freedom is its
//! own responsibility, and this is what checks it.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let chunks = pb_decode::read_png_text(data);
    // Bound-check the caps the parser promises, so a regression that lets a
    // zlib bomb through is a fuzz failure rather than a silent OOM later.
    for (keyword, text) in &chunks {
        assert!(!keyword.is_empty() && keyword.len() <= 79);
        assert!(text.len() <= 1 << 20);
    }
});
