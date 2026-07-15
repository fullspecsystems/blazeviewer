//! Measure the per-snapshot cost the streaming folder scan pays off-thread.
//!
//! The streaming scan publishes a growing playlist snapshot every ~150 ms; each one clones
//! the cumulative path list and builds a fresh `FsSource` (which recomputes a display name
//! per path, `FsSource::new`). That work runs on the **scan worker thread**, never the event
//! loop — but its total is O(N × number-of-batches), so this example confirms it stays cheap
//! even for very large folders (the plan's "measure, don't assert" item).
//!
//! Run: `cargo run --release -p pb-source --example fssource_bench`
//!
//! Dependency-free (no Criterion): a single timed pass per size. Indicative, not a rigorous
//! microbench — but enough to see whether a million-path snapshot is milliseconds (fine,
//! off-thread) or seconds (escalate to a segmented append-only buffer, per the plan).

use std::path::PathBuf;
use std::time::Instant;

use pb_source::{FsSource, ItemSource};

fn synthetic_paths(n: usize) -> Vec<PathBuf> {
    // A realistic-ish nested layout so `display_name` (which inspects the path) does real work.
    (0..n)
        .map(|i| PathBuf::from(format!("/photos/library/{:04}/IMG_{:08}.jpg", i / 200, i)))
        .collect()
}

fn main() {
    println!(
        "{:>10}  {:>12}  {:>12}  {:>12}",
        "paths", "clone (ms)", "FsSource (ms)", "total (ms)"
    );
    for &n in &[10_000usize, 100_000, 1_000_000] {
        let cumulative = synthetic_paths(n);

        // (1) Cloning the cumulative Vec — what build_resolved does before FsSource::new.
        let t = Instant::now();
        let cloned = cumulative.clone();
        let clone_ms = t.elapsed().as_secs_f64() * 1e3;

        // (2) Building the snapshot (recomputes the display-name list).
        let t = Instant::now();
        let src = FsSource::new(cloned);
        let build_ms = t.elapsed().as_secs_f64() * 1e3;

        // Touch the result so the optimizer can't elide the build.
        std::hint::black_box(src.len());

        println!(
            "{:>10}  {:>12.2}  {:>12.2}  {:>12.2}",
            n,
            clone_ms,
            build_ms,
            clone_ms + build_ms
        );
    }
    println!(
        "\nPer-batch cost ≈ the row for the cumulative size so far, paid on the scan worker \
         thread (~every 150 ms). If even the 1M row is a few ms, the off-thread snapshot-swap \
         is comfortably cheap; if it's hundreds of ms, switch to a segmented append-only buffer."
    );
}
