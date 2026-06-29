//! Read-only structural inspection + **bounded** timing of a real `.7z`, to diagnose a
//! slow open without decompressing the whole archive. Reports file/block count, solid
//! vs non-solid, and encryption from the header, then times a small sample of blocks at
//! the **start** and near the **middle** — so a flat per-block cost (an AES key
//! derivation per block) and a *growing* cost (an O(N²)/positional effect) are
//! distinguishable, and the full open time is extrapolated.
//!
//! Safe: opens read-only, decodes only ~a couple hundred blocks total, never the whole
//! archive into RAM.
//!
//! ```sh
//! cargo run --release -p pb-source --example archive_inspect -- <path.7z> [password]
//! ```

use std::fs::File;
use std::io::BufReader;
use std::time::Instant;

use sevenz_rust2::{Archive, BlockDecoder, EncoderMethod, Password};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("usage: archive_inspect <path.7z> [password]");
    let password = args.next();
    let pwd = match &password {
        Some(p) => Password::from(p.as_str()),
        None => Password::empty(),
    };

    let mut f = BufReader::new(File::open(&path).expect("open archive"));
    let archive = Archive::read(&mut f, &pwd).expect("read header (wrong/missing password?)");

    let files = archive
        .files
        .iter()
        .filter(|x| !x.is_directory() && x.has_stream())
        .count();
    let blocks = archive.blocks.len();
    let encrypted = archive.blocks.iter().any(|b| {
        b.coders
            .iter()
            .any(|c| c.encoder_method_id() == EncoderMethod::ID_AES256_SHA256)
    });
    let no_block_crc = archive.blocks.iter().filter(|b| !b.has_crc).count();

    println!("path:          {path}");
    println!("files:         {files}");
    println!(
        "blocks:        {blocks}   ({})",
        if archive.is_solid {
            "solid"
        } else {
            "NON-SOLID"
        }
    );
    println!("encrypted:     {encrypted}");
    println!("files/block:   {:.2}", files as f64 / blocks.max(1) as f64);
    println!(
        "blocks w/o block-level CRC: {no_block_crc}  (single-substream ones can hit the O(N^2) scan)"
    );

    if blocks < 4 {
        println!("\nFew blocks -> this is not the many-small-blocks case; slowness is elsewhere.");
        return;
    }

    // Bounded timed sample: decode K blocks at the start and K near the middle.
    let k = 100usize.min(blocks / 4).max(1);
    let time_window = |start: usize, f: &mut BufReader<File>| -> f64 {
        let t = Instant::now();
        for bi in start..(start + k) {
            let dec = BlockDecoder::new(1, bi, &archive, &pwd, f);
            dec.for_each_entries(&mut |_e, r| {
                let mut b = [0u8; 8192];
                while r.read(&mut b)? > 0 {}
                Ok(true)
            })
            .expect("decode block");
        }
        t.elapsed().as_secs_f64() / k as f64
    };

    let start_ms = time_window(0, &mut f) * 1e3;
    let mid_ms = time_window(blocks / 2, &mut f) * 1e3;
    let avg = (start_ms + mid_ms) / 2.0;

    println!("\nsampled {k} blocks at each position:");
    println!("  per-block decode  start: {start_ms:7.1} ms   middle: {mid_ms:7.1} ms");
    println!(
        "  extrapolated full open: {:.1} min   ({blocks} blocks x ~{avg:.1} ms)",
        avg * blocks as f64 / 60_000.0
    );
    if mid_ms > start_ms * 1.5 {
        println!(
            "  -> middle MUCH slower than start: cost GROWS with position (O(N^2)/positional)."
        );
    } else {
        println!("  -> start ~= middle: flat per-block cost (one AES key derivation per block).");
    }
}
