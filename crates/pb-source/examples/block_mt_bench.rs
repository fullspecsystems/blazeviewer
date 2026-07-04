//! A/B bench for the 7z eager-open thread strategy (Task 30 follow-up; the
//! "measure, don't guess" rule).
//!
//! `SevenZSource::open_with_progress` parallelizes **across blocks** (outer) with a
//! within-block decoder thread count of 1 (inner). That's optimal for a non-solid
//! archive (1336 one-file blocks), but a **solid** archive is one giant LZMA2
//! block: outer parallelism has nothing to chew on and the whole open runs on one
//! core. `lzma-rust2` ships `Lzma2ReaderMt`, which decodes the independent
//! (dict-reset) chunks a 7-Zip *multithread-compressed* stream contains in
//! parallel — this probe measures whether handing `BlockDecoder` more inner
//! threads actually closes the solid-archive gap, and what it costs in RAM.
//!
//! ```sh
//! cargo run --release -p pb-source --example block_mt_bench -- <archive.7z> [password|-] [inner_threads] [all|images]
//! ```
//!
//! `inner_threads` defaults to 1 (the shipped behavior). `all` decodes every
//! streamed entry (apples-to-apples with `7z t`); `images` (default) mirrors the
//! app's image-extension filter.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use sevenz_rust2::{Archive, BlockDecoder, Password};

fn is_image(ext: &str) -> bool {
    matches!(
        ext,
        "jpg"
            | "jpeg"
            | "jpe"
            | "jfif"
            | "png"
            | "gif"
            | "bmp"
            | "tif"
            | "tiff"
            | "webp"
            | "heic"
            | "heif"
            | "avif"
            | "jxl"
            | "qoi"
            | "tga"
            | "svg"
            | "arw"
            | "nef"
            | "cr2"
            | "cr3"
            | "dng"
            | "raf"
            | "rw2"
            | "orf"
    )
}

fn ext_of(name: &str) -> String {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("usage: block_mt_bench <archive.7z> [password|-] [inner_threads] [all|images]");
    let password = args.next().filter(|p| p != "-");
    let inner: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    let all = args.next().as_deref() == Some("all");

    let pw = match &password {
        Some(p) => Password::from(p.as_str()),
        None => Password::empty(),
    };

    let archive = {
        let mut f = BufReader::with_capacity(1 << 20, File::open(&path).expect("open"));
        Archive::read(&mut f, &pw).expect("read header")
    };
    let block_count = archive.blocks.len();

    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    // Outer threads share the cores with each decoder's inner workers.
    let outer = cores
        .div_ceil(inner as usize)
        .min(block_count.max(1))
        .max(1);

    println!(
        "{}: {} blocks | outer {} x inner {} ({} cores) | {} entries",
        path,
        block_count,
        outer,
        inner,
        cores,
        if all { "ALL" } else { "image" },
    );

    let next = AtomicUsize::new(0);
    let peak_before = peak_working_set().unwrap_or(0);
    let t = Instant::now();

    let decoded: u64 = std::thread::scope(|s| {
        let handles: Vec<_> = (0..outer)
            .map(|_| {
                s.spawn(|| {
                    let mut local = 0u64;
                    let mut f = BufReader::with_capacity(1 << 20, File::open(&path).expect("open"));
                    let mut sink = vec![0u8; 1 << 20];
                    loop {
                        let bi = next.fetch_add(1, Ordering::Relaxed);
                        if bi >= block_count {
                            break;
                        }
                        let dec = BlockDecoder::new(inner, bi, &archive, &pw, &mut f);
                        // Skip blocks with no wanted entries (mirrors the app's pre-scan).
                        if !all
                            && !dec.entries().iter().any(|e| {
                                !e.is_directory() && e.has_stream() && is_image(&ext_of(e.name()))
                            })
                        {
                            continue;
                        }
                        dec.for_each_entries(&mut |entry, rd| {
                            if entry.is_directory() || !entry.has_stream() {
                                return Ok(true);
                            }
                            loop {
                                let n = rd.read(&mut sink)?;
                                if n == 0 {
                                    break;
                                }
                                local += n as u64;
                            }
                            Ok(true)
                        })
                        .expect("block decode");
                    }
                    local
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).sum()
    });

    let secs = t.elapsed().as_secs_f64();
    let peak_after = peak_working_set().unwrap_or(0);
    println!(
        "decoded {:.0} MB in {:.2}s = {:.0} MB/s | peak RSS {:.0} MB (was {:.0} MB)",
        decoded as f64 / 1e6,
        secs,
        decoded as f64 / 1e6 / secs.max(1e-9),
        peak_after as f64 / 1e6,
        peak_before as f64 / 1e6,
    );
}

#[cfg(windows)]
fn peak_working_set() -> Option<u64> {
    #[repr(C)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }
    extern "system" {
        fn GetCurrentProcess() -> isize;
        fn K32GetProcessMemoryInfo(
            process: isize,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
    }
    // SAFETY: zeroed POD with `cb` set; our own process handle; read only on success.
    unsafe {
        let mut c: ProcessMemoryCounters = std::mem::zeroed();
        c.cb = std::mem::size_of::<ProcessMemoryCounters>() as u32;
        if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) != 0 {
            Some(c.peak_working_set_size as u64)
        } else {
            None
        }
    }
}

#[cfg(not(windows))]
fn peak_working_set() -> Option<u64> {
    None
}
