//! Eager-7z open probe — measures the open so the RAM budget in
//! `pb-app/src/archive.rs` can be set from data instead of guessed (Task 30 #5;
//! the project's "measure, don't guess" prime directive).
//!
//! For a `.7z` it reports, with timings:
//!   - **projected resident bytes** — the header sum [`seven_z_projected_bytes`]
//!     uses for the pre-flight (no decompression); this is what the budget gates.
//!   - the **eager open** wall time — the work the app runs on a background thread
//!     in `App::begin_archive_open`; confirms async is required.
//!   - the process **peak working set** right after the load (real physical RAM at
//!     the high-water mark), and the **transient overhead** above the resident
//!     projection. That overhead is what `TRANSIENT_MARGIN` in `archive.rs` must
//!     cover; the ratio tells us whether a flat margin or a fraction of resident
//!     is the right model.
//!
//! Run on a representative *solid* photo archive (already-compressed JPEGs are the
//! pathological case for a solid 7z — full decompress cost, ~zero space saved):
//!
//! ```sh
//! cargo run --release -p pb-source --example archive_probe -- path\to\album.7z
//! ```
//!
//! Note: this isolates the *archive* cost (resident bytes + decode-time transient).
//! The running app's total peak also holds the GPU texture ring (~1.5 GB) and the
//! decode pool (512 MB) — accounted separately as `APP_RESERVATIONS` — plus the
//! per-decode `bytes()` clone (bounded by decode concurrency × the largest entry).

use std::path::Path;
use std::time::Instant;

use pb_source::{seven_z_projected_bytes, OpenProgress, PhotoSource, SevenZSource};

/// The image extensions the app decodes — a mirror of
/// `pb_decode::is_supported_extension` for the common photo formats, enough to
/// select the entries in a real photo archive. (pb-source has no decode dep, so
/// the predicate is duplicated here for the example only.)
fn is_supported(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
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

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: archive_probe <archive.7z> [password] [mt_headroom_gb]");
        std::process::exit(2);
    });
    let password = std::env::args().nth(2).filter(|p| p != "-");
    // Transient RAM the open may spend on within-block MT decode (the solid-archive
    // speedup), in GB. Default 0 = the historical single-threaded-per-block path;
    // the app passes its real unused budget here.
    let mt_headroom = std::env::args()
        .nth(3)
        .and_then(|s| s.parse::<f64>().ok())
        .map(|gb| (gb * 1024.0 * 1024.0 * 1024.0) as u64)
        .unwrap_or(0);
    let path = Path::new(&arg);
    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    // 1) Pre-flight projection: header read only, no decompression.
    let t = Instant::now();
    let projected = seven_z_projected_bytes(path, password.as_deref(), is_supported)
        .expect("header read / projection failed");
    let project_ms = t.elapsed().as_secs_f64() * 1000.0;

    let peak_before = peak_working_set();

    // 2) Eager open: decompress every supported image into RAM (the slow step).
    let t = Instant::now();
    let src = SevenZSource::open_with_progress(
        path,
        password.clone(),
        is_supported,
        Some(&OpenProgress::new()),
        mt_headroom,
    )
    .expect("eager open failed");
    let open_s = t.elapsed().as_secs_f64();
    let peak_after = peak_working_set(); // sample before any bytes() clone inflates it
    let count = src.len();

    println!("archive:              {}", path.display());
    println!("file on disk:         {:.0} MB", mb(file_size));
    println!("images:               {count}");
    println!(
        "projected resident:   {:.0} MB   (header sum, {project_ms:.1} ms, no decompress)",
        mb(projected)
    );
    println!(
        "eager open:           {open_s:.2} s   ({:.0} MB/s decompressed)",
        mb(projected) / open_s.max(1e-9)
    );
    match (peak_before, peak_after) {
        (Some(before), Some(after)) => {
            let transient = after.saturating_sub(projected);
            let pct = transient as f64 / (projected.max(1)) as f64 * 100.0;
            println!(
                "peak working set:     {:.0} MB   (baseline {:.0} MB before open)",
                mb(after),
                mb(before)
            );
            println!(
                "transient over resident: {:.0} MB   ({pct:.0}% of resident)",
                mb(transient)
            );
            println!(
                "peak / resident:      {:.2}x",
                after as f64 / (projected.max(1)) as f64
            );
        }
        _ => println!("peak working set:     (unavailable on this platform)"),
    }

    // Hold the resident map until after the peak sample above.
    drop(src);
}

/// Process peak working set (the max physical RAM ever resident), in bytes. `None`
/// off Windows. `K32GetProcessMemoryInfo` is exported by kernel32 (linked by
/// default), so this needs no extra crate or link directive.
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
    // SAFETY: zeroed POD with `cb` set as the API requires; we pass our own process
    // handle and a correctly-sized buffer, and only read the field on success.
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
