//! Why is a *password-protected* 7z so much slower to open than a plain one?
//!
//! This probe builds a plain and an AES-encrypted **solid-LZMA2** archive from the
//! same incompressible (JPEG-like) bytes, then times a full decode of each under
//! several input strategies and reports MB/s. It exists to settle the question with
//! numbers (the project's "measure, don't guess" rule) before we change any code.
//!
//! Hypothesis under test: `sevenz_rust2`'s `Aes256Sha256Decoder` reads its input in
//! **512-byte chunks** straight off the *unbuffered* `File` (`decoder.rs` wraps the
//! file only in a size-limiting `BoundedReader`, no `BufReader`). So an encrypted
//! stream is pulled off disk in millions of tiny syscalls, while the plain LZMA2 path
//! reads in large chunks and decodes multi-threaded. Wrapping the file in a
//! `BufReader` before handing it to the reader should close most of the gap — that's
//! the candidate fix, tested here as a variant.
//!
//! Safe by construction: it builds its own small archive in the temp dir (default
//! 256 MB, override with an arg) and cleans up. It never touches real archives.
//!
//! ```sh
//! cargo run --release -p pb-source --example aes_throughput -- 256
//! ```

use std::fs::File;
use std::io::{BufReader, Read, Seek, Write};
use std::time::Instant;

use sevenz_rust2::{compress_to_path, compress_to_path_encrypted, ArchiveReader, Password};

const PASSWORD: &str = "test";

fn main() {
    let total_mb: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(256);
    let files = 50usize;
    let per = total_mb * 1024 * 1024 / files;
    let total_bytes = (per * files) as u64;

    // 1) Build an incompressible corpus (xorshift-filled ".jpg" files) in the temp dir.
    let dir = std::env::temp_dir().join(format!("pb_aes_probe_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut st = 0x9E3779B97F4A7C15u64;
    let mut buf = vec![0u8; per];
    for i in 0..files {
        for b in buf.iter_mut() {
            // xorshift64* — incompressible enough that LZMA2 can't shrink it, so the
            // decode is dominated by stream reading (the AES path's weak spot).
            st ^= st >> 12;
            st ^= st << 25;
            st ^= st >> 27;
            *b = (st.wrapping_mul(0x2545F4914F6CDD1D) >> 56) as u8;
        }
        File::create(dir.join(format!("img{i:03}.jpg")))
            .unwrap()
            .write_all(&buf)
            .unwrap();
    }

    let plain = std::env::temp_dir().join(format!("pb_aes_{}_plain.7z", std::process::id()));
    let enc = std::env::temp_dir().join(format!("pb_aes_{}_enc.7z", std::process::id()));

    let t = Instant::now();
    compress_to_path(&dir, &plain).expect("build plain 7z");
    let plain_build = t.elapsed().as_secs_f64();
    let t = Instant::now();
    compress_to_path_encrypted(&dir, &enc, Password::from(PASSWORD)).expect("build enc 7z");
    let enc_build = t.elapsed().as_secs_f64();

    let plain_sz = std::fs::metadata(&plain).map(|m| m.len()).unwrap_or(0);
    let enc_sz = std::fs::metadata(&enc).map(|m| m.len()).unwrap_or(0);

    println!("corpus: {files} files, {total_mb} MB uncompressed (incompressible / JPEG-like)");
    println!(
        "plain.7z {:.0} MB built in {plain_build:.1}s | enc.7z {:.0} MB built in {enc_build:.1}s",
        plain_sz as f64 / 1e6,
        enc_sz as f64 / 1e6
    );
    println!(
        "threads available: {}",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    );
    println!();
    println!("{:<40} {:>9} {:>9}", "variant", "time", "MB/s");
    println!("{}", "-".repeat(60));

    // 2) Decode variants. Each opens its own handle, decodes every entry fully.
    let plain_mbps = bench("plain | raw File (baseline)", total_bytes, || {
        let mut rd = ArchiveReader::new(File::open(&plain).unwrap(), Password::empty()).unwrap();
        drain_all(&mut rd)
    });
    bench("ENCRYPTED | raw File (the bug)", total_bytes, || {
        let mut rd =
            ArchiveReader::new(File::open(&enc).unwrap(), Password::from(PASSWORD)).unwrap();
        drain_all(&mut rd)
    });
    let fix_mbps = bench("ENCRYPTED | BufReader 1 MiB (fix?)", total_bytes, || {
        let r = BufReader::with_capacity(1 << 20, File::open(&enc).unwrap());
        let mut rd = ArchiveReader::new(r, Password::from(PASSWORD)).unwrap();
        drain_all(&mut rd)
    });
    bench("ENCRYPTED | BufReader + 1 thread", total_bytes, || {
        let r = BufReader::with_capacity(1 << 20, File::open(&enc).unwrap());
        let mut rd = ArchiveReader::new(r, Password::from(PASSWORD)).unwrap();
        rd.set_thread_count(1);
        drain_all(&mut rd)
    });

    println!("{}", "-".repeat(60));
    println!(
        "fix recovers {:.0}% of plain-archive throughput",
        fix_mbps / plain_mbps.max(1e-9) * 100.0
    );

    // Cleanup.
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&plain);
    let _ = std::fs::remove_file(&enc);
}

/// Decode every entry in full, returning the total uncompressed bytes read.
fn drain_all<R: Read + Seek>(rd: &mut ArchiveReader<R>) -> u64 {
    let mut total = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    rd.for_each_entries(|_entry, r| {
        loop {
            let n = r.read(&mut buf)?; // io::Error -> sevenz_rust2::Error via From
            if n == 0 {
                break;
            }
            total += n as u64;
        }
        Ok(true)
    })
    .expect("decode failed");
    total
}

/// Time a decode closure, print `time` + `MB/s`, return the MB/s.
fn bench(name: &str, expect_bytes: u64, f: impl FnOnce() -> u64) -> f64 {
    let t = Instant::now();
    let decoded = f();
    let s = t.elapsed().as_secs_f64();
    let mbps = (decoded as f64 / 1e6) / s.max(1e-9);
    let warn = if decoded == expect_bytes {
        ""
    } else {
        "  <-- byte mismatch!"
    };
    println!("{name:<40} {s:>8.2}s {mbps:>9.0}{warn}");
    mbps
}
