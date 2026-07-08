//! THROWAWAY correctness check: decode an HEIC with BOTH the WIC and libheif
//! backends and compare. Confirms orientation (dims match) and color state (a
//! large systematic pixel delta on a Display-P3 file would mean one backend
//! gamut-mapped to sRGB while the other kept native P3 — the color-parity risk).
//!
//!   cargo run -q --release --example heic_compare -p pb-decode --features libheif -- <file>
//!
//! Calls the backends directly (not the dispatcher) so both run in one process.

#[cfg(windows)]
use pb_decode::{DecodeRequest, ImageDecoder, LibHeifDecoder, WicDecoder};
#[cfg(windows)]
use std::path::Path;

// Decodes with BOTH WIC and libheif to compare them — inherently Windows-only (no
// WIC anywhere else). Off Windows this is a stub (`main` at the bottom); the libheif
// backend alone is smoke-tested with `--example decode`.
#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: heic_compare <file>");
        return;
    }
    let bytes = std::fs::read(Path::new(&args[0])).expect("read");
    let req = DecodeRequest {
        bytes: &bytes,
        fit: None, // full res, both backends, so the diff is apples-to-apples
        allow_preview: false,
    };

    let wic = WicDecoder.decode(&req).expect("wic decode");
    let lib = LibHeifDecoder.decode(&req).expect("libheif decode");

    println!(
        "WIC     {}x{}  color.enabled={}",
        wic.orig_width, wic.orig_height, wic.color.enabled
    );
    println!(
        "libheif {}x{}  color.enabled={}",
        lib.orig_width, lib.orig_height, lib.color.enabled
    );
    println!(
        "orientation match: {}",
        (wic.orig_width, wic.orig_height) == (lib.orig_width, lib.orig_height)
    );
    println!(
        "color-transform match: enabled {}=={}, m00 {:.4} vs {:.4}",
        wic.color.enabled, lib.color.enabled, wic.color.matrix[0][0], lib.color.matrix[0][0]
    );

    if (wic.width, wic.height) != (lib.width, lib.height) {
        println!(
            "DIMENSION MISMATCH — cannot pixel-diff ({}x{} vs {}x{})",
            wic.width, wic.height, lib.width, lib.height
        );
        return;
    }

    // Per-channel range stats. A YCbCr full-vs-limited-range mismatch shows up as
    // one backend's output compressed toward [16,235] (washed out) while the other
    // uses the full [0,255]. Whichever spans the full range on a normal photo is the
    // more plausibly-correct one.
    let stats = |px: &[u8], ch: usize| -> (u8, u8, f64) {
        let (mut lo, mut hi, mut sum, mut n) = (255u8, 0u8, 0u64, 0u64);
        for p in px.chunks_exact(4) {
            let v = p[ch];
            lo = lo.min(v);
            hi = hi.max(v);
            sum += v as u64;
            n += 1;
        }
        (lo, hi, sum as f64 / n as f64)
    };
    println!("\nper-channel range (min..max, mean):");
    for (name, ch) in [("R", 0), ("G", 1), ("B", 2)] {
        let (wl, wh, wm) = stats(&wic.pixels, ch);
        let (ll, lh, lm) = stats(&lib.pixels, ch);
        println!(
            "  {name}: WIC {:>3}..{:<3} m{:>6.1}   |   libheif {:>3}..{:<3} m{:>6.1}",
            wl, wh, wm, ll, lh, lm
        );
    }

    // Per-channel mean + max absolute difference over RGB (ignore A).
    let n = wic.pixels.len().min(lib.pixels.len());
    let (mut sum, mut max, mut count) = (0u64, 0u8, 0u64);
    let mut hist = [0u64; 8]; // bucket diffs: 0,1-2,3-4,5-8,9-16,17-32,33-64,65+
    for i in 0..n {
        if i % 4 == 3 {
            continue; // skip alpha
        }
        let d = wic.pixels[i].abs_diff(lib.pixels[i]);
        sum += d as u64;
        if d > max {
            max = d;
        }
        count += 1;
        let b = match d {
            0 => 0,
            1..=2 => 1,
            3..=4 => 2,
            5..=8 => 3,
            9..=16 => 4,
            17..=32 => 5,
            33..=64 => 6,
            _ => 7,
        };
        hist[b] += 1;
    }
    let mean = sum as f64 / count as f64;
    println!(
        "\nRGB pixel delta (WIC vs libheif): mean {:.3} levels, max {}",
        mean, max
    );
    let pct = |x: u64| 100.0 * x as f64 / count as f64;
    println!(
        "  diff distribution: 0:{:.1}%  1-2:{:.1}%  3-4:{:.1}%  5-8:{:.1}%  9-16:{:.1}%  17-32:{:.1}%  33-64:{:.1}%  65+:{:.1}%",
        pct(hist[0]), pct(hist[1]), pct(hist[2]), pct(hist[3]), pct(hist[4]), pct(hist[5]), pct(hist[6]), pct(hist[7])
    );
    println!(
        "\nverdict: {}",
        if mean < 3.0 {
            "MATCH (small decoder/chroma differences only — same color space)"
        } else if mean < 12.0 {
            "CLOSE (minor systematic difference — inspect)"
        } else {
            "MISMATCH (likely a color-space discrepancy — investigate)"
        }
    );
}

/// Off Windows there's no WIC to compare against, so this A/B tool is a no-op.
#[cfg(not(windows))]
fn main() {
    eprintln!(
        "heic_compare compares libheif against WIC and is Windows-only; \
         use `--example decode` to smoke-test the libheif backend elsewhere."
    );
}
