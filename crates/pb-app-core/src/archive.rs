//! Archive-open policy: the RAM budget for eagerly-decompressed archives (7z) and
//! the structured failure type the open path reports.
//!
//! Why a budget: a 7z is decompressed whole into RAM on open (solid archives have
//! no cheap random access). A real allocation failure in Rust *aborts* the process
//! (uncatchable), so we must **predict and refuse** an archive that won't fit
//! before we start, rather than try-and-catch. The prediction is
//! `pb_source::seven_z_projected_bytes` (the archive's resident decompressed-image
//! bytes); this module decides the ceiling to compare it against.
//!
//! The budget is a *fraction* of the machine's currently-available physical RAM
//! (queried at open time, never a hardcoded number), minus what PhotoBlaze already
//! reserves and a margin for transient copies. `PB_ARCHIVE_RAM_BUDGET` overrides it
//! (for testing the refusal path on a big-RAM box, and for power users).
//!
//! Measured (Task 30 #5, `cargo run --release -p pb-source --example archive_probe`)
//! on solid-LZMA2 photo archives (already-compressed JPEGs — the pathological case):
//! the projection predicts real RAM closely (process peak working set was 1.02–1.26x
//! the resident projection), the eager-open transient stayed ~60–200 MB *regardless
//! of archive size* (a fixed decoder cost, not proportional — the 3.3 GB archive
//! actually overshot less than the 0.7 GB one), and decompression ran ~1 GB/s (so a
//! multi-GB open takes seconds, which is why it runs async, off the event loop). The
//! fraction + margin below are set conservatively from that data.

/// Fraction of *available* physical RAM an eager archive's resident bytes may use,
/// leaving the rest for the OS and other apps. The pre-flight projection is a close
/// predictor of real RAM (measured peak/resident 1.02–1.26x), so the gate is
/// trustworthy; 0.6 keeps a comfortable margin against that small overshoot plus the
/// transient copies (the per-decode `bytes()` clone, decode-pool RGBA, the GPU ring)
/// that ride on top of the resident projection.
const BUDGET_FRACTION: f64 = 0.6;
/// What PhotoBlaze itself already reserves, subtracted from the archive budget:
/// the GPU texture ring (~1.5 GB) + the decode pool (512 MB). Mirrors
/// `RING_BUDGET_BYTES` + `POOL_BUDGET_BYTES` in `main.rs`.
const APP_RESERVATIONS: u64 = 1_500_000_000 + 512 * 1024 * 1024;
/// Headroom for transient copies not counted in the resident projection: the eager
/// open's own decoder scratch (measured 60–200 MB, flat across archive size) plus the
/// live-viewing per-decode `bytes()` clones (decode concurrency × the largest entry).
/// 512 MB comfortably covers the measured overhead.
const TRANSIENT_MARGIN: u64 = 512 * 1024 * 1024;
/// Fallback when physical RAM can't be queried (an unsupported platform, or the query
/// fails). Windows + macOS both query it for real; this covers everything else.
const ASSUMED_RAM: u64 = 8 * 1024 * 1024 * 1024;

/// Available physical RAM in bytes, queried now. `None` if it can't be determined.
#[cfg(windows)]
pub fn available_physical_ram() -> Option<u64> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: zeroed POD; `dwLength` set as the API requires before the call fills it.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    unsafe { GlobalMemoryStatusEx(&mut status).ok()? };
    Some(status.ullAvailPhys)
}

/// Available physical RAM in bytes — the macOS analog of Windows' `ullAvailPhys`:
/// pages the kernel can hand out without swapping (free + inactive + speculative),
/// times the page size, via the Mach `host_statistics64(HOST_VM_INFO64)`. `None` on an
/// unexpected Mach failure (then [`ram_budget`] falls back to `ASSUMED_RAM`).
#[cfg(target_os = "macos")]
// `mach_host_self` is deprecated in libc 0.2 in favor of the `mach2` crate; we keep the
// (still-functional) libc binding rather than add a whole dependency for one host-port
// call. Scoped to this fn so real deprecations elsewhere still surface under `-D warnings`.
#[allow(deprecated)]
pub fn available_physical_ram() -> Option<u64> {
    // SAFETY: `host_statistics64` fills the zeroed `vm_statistics64` and returns
    // `KERN_SUCCESS` only when it wrote `count` `integer_t`s — we pass exactly the
    // struct's size in those units, the count the kernel expects for this flavor.
    // The host-port send right from `mach_host_self()` is a special, process-lifetime
    // port; libc 0.2 doesn't expose `mach_port_deallocate`, and not balancing one
    // reference per (rare) archive open is a negligible, process-bounded non-leak.
    unsafe {
        let mut stats: libc::vm_statistics64 = std::mem::zeroed();
        let mut count = (std::mem::size_of::<libc::vm_statistics64>()
            / std::mem::size_of::<libc::integer_t>())
            as libc::mach_msg_type_number_t;
        let kr = libc::host_statistics64(
            libc::mach_host_self(),
            libc::HOST_VM_INFO64,
            &mut stats as *mut libc::vm_statistics64 as libc::host_info64_t,
            &mut count,
        );
        if kr != libc::KERN_SUCCESS {
            return None;
        }
        let page = libc::sysconf(libc::_SC_PAGESIZE);
        if page <= 0 {
            return None;
        }
        // Reclaimable without swap. Skip `purgeable_count` (it can overlap the
        // active/inactive tallies) to avoid over-counting → an over-generous budget.
        let reclaimable = u64::from(stats.free_count)
            + u64::from(stats.inactive_count)
            + u64::from(stats.speculative_count);
        Some(reclaimable.saturating_mul(page as u64))
    }
}

/// Available physical RAM in bytes on Linux: the kernel's own `MemAvailable`
/// estimate (free + reclaimable without swapping) from `/proc/meminfo`. Added
/// in plan #102 rev2 — tarballs are Linux-first, and falling back to the
/// assumed-8-GB figure would undermine the budget gate on exactly the platform
/// the eager tarball open targets.
#[cfg(target_os = "linux")]
pub fn available_physical_ram() -> Option<u64> {
    parse_meminfo_available(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

/// Parse the `MemAvailable:` line (reported in kB) out of `/proc/meminfo` text.
/// Split out so the parse is unit-testable on every platform.
#[cfg(any(target_os = "linux", test))]
fn parse_meminfo_available(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find(|l| l.starts_with("MemAvailable:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb.saturating_mul(1024))
}

/// Available physical RAM in bytes (stub on platforms without a query wired up).
#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
pub fn available_physical_ram() -> Option<u64> {
    None
}

/// The byte ceiling for an eager archive's resident decompressed-image bytes. The
/// `PB_ARCHIVE_RAM_BUDGET` env override wins (e.g. `512MB`, `2gb`, `1.5GB`, or raw
/// bytes); otherwise it's `(available - reservations - margin) * fraction` (see
/// [`budget_from`] for why the reservation is subtracted before the fraction).
pub fn ram_budget() -> u64 {
    if let Ok(s) = std::env::var("PB_ARCHIVE_RAM_BUDGET") {
        if let Some(b) = parse_budget(&s) {
            return b;
        }
        eprintln!(
            "{}: ignoring unparseable PB_ARCHIVE_RAM_BUDGET={s:?}",
            crate::APP_NAME
        );
    }
    let available = available_physical_ram().unwrap_or(ASSUMED_RAM);
    budget_from(
        available,
        APP_RESERVATIONS,
        BUDGET_FRACTION,
        TRANSIENT_MARGIN,
    )
}

/// The budget math, pulled out so it's unit-testable without touching the OS.
///
/// Reserve *first*, then take the fraction: `(available - reservations - margin) *
/// fraction`. The fraction is a headroom cushion on the RAM that's actually *spare*
/// after PhotoBlaze's own resident use (ring + pool) and the transient-copy margin —
/// not a slice of gross RAM the reservations are then subtracted from again. The old
/// order (`available * fraction - reservations - margin`) double-penalized modest
/// machines: an 8 GB box with ~4 GB free computed a **0**-byte budget and refused
/// every archive, even a 1 MB one. Reserve-first yields ~0.9 GB there, while still
/// saturating to 0 (correctly refusing) once free RAM drops below the reservations.
fn budget_from(available: u64, reservations: u64, fraction: f64, margin: u64) -> u64 {
    let spare = available
        .saturating_sub(reservations)
        .saturating_sub(margin);
    (spare as f64 * fraction) as u64
}

/// Parse a `PB_ARCHIVE_RAM_BUDGET` value: an integer or decimal with an optional
/// `KB`/`MB`/`GB` (or `K`/`M`/`G`) suffix (case-insensitive), else raw bytes.
fn parse_budget(s: &str) -> Option<u64> {
    let lower = s.trim().to_ascii_lowercase();
    let (num, mult): (&str, u64) =
        if let Some(n) = lower.strip_suffix("gb").or(lower.strip_suffix('g')) {
            (n, 1024 * 1024 * 1024)
        } else if let Some(n) = lower.strip_suffix("mb").or(lower.strip_suffix('m')) {
            (n, 1024 * 1024)
        } else if let Some(n) = lower.strip_suffix("kb").or(lower.strip_suffix('k')) {
            (n, 1024)
        } else {
            (lower.as_str(), 1)
        };
    let num = num.trim();
    if let Ok(v) = num.parse::<u64>() {
        return Some(v.saturating_mul(mult));
    }
    match num.parse::<f64>() {
        Ok(f) if f >= 0.0 => Some((f * mult as f64) as u64),
        _ => None,
    }
}

/// A human, decimal-GB (or MB) size for user-facing copy, e.g. `6.2 GB`.
pub fn human_gb(bytes: u64) -> String {
    let gb = bytes as f64 / 1_000_000_000.0;
    if gb >= 0.1 {
        format!("{gb:.1} GB")
    } else {
        format!("{:.0} MB", bytes as f64 / 1_000_000.0)
    }
}

/// Why opening an archive failed, in app terms (distinct cases get distinct
/// messages — the error dialog and the log both use [`user_message`]).
///
/// [`user_message`]: ArchiveOpenError::user_message
#[derive(Debug)]
pub enum ArchiveOpenError {
    /// The archive's projected resident size exceeds the RAM budget; refused before
    /// loading. `needed` is the projection, `budget` the ceiling.
    TooLarge { needed: u64, budget: u64 },
    /// A `try_reserve` shortfall while decompressing into RAM.
    OutOfMemory,
    /// Damaged, truncated, or an unsupported compression method.
    Corrupt,
    /// Encrypted (incl. encrypted header) with no password; not supported yet.
    PasswordRequired,
    /// Opened fine but holds no supported images.
    Empty,
    /// An I/O error opening or reading the file.
    Io(String),
    /// The user cancelled the open before it finished. Not really an error — the app
    /// drops it quietly (no failure dialog), keeping whatever was on screen.
    Cancelled,
}

impl ArchiveOpenError {
    /// A plain, em-dash-free, one-line message for the user (error dialog + log).
    pub fn user_message(&self) -> String {
        match self {
            ArchiveOpenError::TooLarge { needed, budget } => format!(
                "You have insufficient memory to open this archive. It needs at least {}, but only {} is available.",
                human_gb(*needed),
                human_gb(*budget)
            ),
            ArchiveOpenError::OutOfMemory => "Ran out of memory while loading this archive.".into(),
            ArchiveOpenError::Corrupt => {
                "This archive cannot be opened. It may be damaged or use unsupported compression."
                    .into()
            }
            ArchiveOpenError::PasswordRequired => {
                "This archive is password protected, which is not supported yet.".into()
            }
            ArchiveOpenError::Empty => "This archive has no images to show.".into(),
            ArchiveOpenError::Io(e) => format!("This archive could not be opened. {e}"),
            ArchiveOpenError::Cancelled => "Archive open cancelled.".into(),
        }
    }
}

impl From<pb_source::OpenError> for ArchiveOpenError {
    fn from(e: pb_source::OpenError) -> Self {
        use pb_source::OpenError as E;
        match e {
            E::Io(io) => ArchiveOpenError::Io(io.to_string()),
            E::Corrupt(_) => ArchiveOpenError::Corrupt,
            E::PasswordRequired => ArchiveOpenError::PasswordRequired,
            E::OutOfMemory => ArchiveOpenError::OutOfMemory,
            E::Cancelled => ArchiveOpenError::Cancelled,
            // A compressed tar has no up-front size table, so its budget check
            // trips mid-stream; `needed` is a lower bound, which is why the
            // user message says "at least".
            E::TooLarge { needed, budget } => ArchiveOpenError::TooLarge { needed, budget },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_subtracts_reservations_and_margin() {
        // Reserve first, then take the fraction: 10 GB available minus 2 GB
        // reservations minus 0.5 GB margin = 7.5 GB spare, of which 60% = ~4.5 GB.
        // Compare against the same f64 path the impl uses (avoids an off-by-one from
        // 0.6 not being exactly representable).
        let gb = 1024 * 1024 * 1024;
        let spare = 10 * gb - 2 * gb - gb / 2;
        let got = budget_from(10 * gb, 2 * gb, 0.6, gb / 2);
        assert_eq!(got, (spare as f64 * 0.6) as u64);
    }

    #[test]
    fn budget_saturates_to_zero_when_reservations_exceed_available() {
        let gb = 1024 * 1024 * 1024;
        // 1 GB available, 4 GB reservations: spare saturates to 0 before the
        // fraction, so a truly memory-starved machine still (correctly) refuses.
        assert_eq!(budget_from(gb, 4 * gb, 0.6, 0), 0);
    }

    #[test]
    fn budget_is_nonzero_on_an_8gb_machine() {
        // Regression: the old `available*fraction - reservations - margin` order
        // floored an 8 GB box (~4 GB free) to 0, refusing every archive. Reserve-first
        // must leave a usable budget there — comfortably more than a small archive.
        let gb = 1024 * 1024 * 1024;
        let got = budget_from(4 * gb, APP_RESERVATIONS, BUDGET_FRACTION, TRANSIENT_MARGIN);
        assert!(
            got >= 512 * 1024 * 1024,
            "8 GB machine should admit at least a 512 MB archive, got {got}"
        );
    }

    #[test]
    fn meminfo_available_parses_the_kernel_line() {
        let text = "MemTotal:       65449180 kB\nMemFree:         8321104 kB\nMemAvailable:   47234872 kB\nBuffers:          812345 kB\n";
        assert_eq!(
            parse_meminfo_available(text),
            Some(47_234_872 * 1024),
            "kB scaled to bytes"
        );
        assert_eq!(parse_meminfo_available("MemTotal: 1 kB\n"), None);
        assert_eq!(parse_meminfo_available("MemAvailable: garbage kB\n"), None);
        assert_eq!(parse_meminfo_available(""), None);
    }

    #[test]
    fn parse_budget_handles_units_and_raw_bytes() {
        assert_eq!(parse_budget("256MB"), Some(256 * 1024 * 1024));
        assert_eq!(parse_budget("2gb"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_budget(" 512m "), Some(512 * 1024 * 1024));
        assert_eq!(
            parse_budget("1.5gb"),
            Some(1024 * 1024 * 1024 + 512 * 1024 * 1024)
        );
        assert_eq!(parse_budget("1073741824"), Some(1_073_741_824));
        assert_eq!(parse_budget("0"), Some(0));
        assert_eq!(parse_budget("garbage"), None);
        assert_eq!(parse_budget(""), None);
    }

    #[test]
    fn human_gb_is_one_decimal() {
        assert_eq!(human_gb(6_200_000_000), "6.2 GB");
        assert_eq!(human_gb(50_000_000), "50 MB");
    }

    // Exercises the real Mach FFI on macOS: it must not crash and must return a sane,
    // page-aligned figure (the failure mode of hand-wired `host_statistics64` is a bad
    // count/flavor → `None` or garbage, not a specific value, so the bounds are loose).
    #[cfg(target_os = "macos")]
    #[test]
    fn available_physical_ram_is_sane_on_macos() {
        let avail = available_physical_ram().expect("Mach host_statistics64 should succeed");
        assert!(avail >= 16 * 1024 * 1024, "implausibly low: {avail}");
        assert!(
            avail < 100 * 1024 * 1024 * 1024 * 1024,
            "implausibly high: {avail}"
        );
        // Page-aligned: the count is multiplied by the page size (16 KiB on Apple Silicon).
        assert_eq!(
            avail % 4096,
            0,
            "should be a whole number of pages: {avail}"
        );
    }

    #[test]
    fn open_error_maps_and_messages() {
        use pb_source::OpenError as E;
        assert!(matches!(
            ArchiveOpenError::from(E::PasswordRequired),
            ArchiveOpenError::PasswordRequired
        ));
        assert!(matches!(
            ArchiveOpenError::from(E::OutOfMemory),
            ArchiveOpenError::OutOfMemory
        ));
        assert!(matches!(
            ArchiveOpenError::from(E::Corrupt("x".into())),
            ArchiveOpenError::Corrupt
        ));
        let msg = (ArchiveOpenError::TooLarge {
            needed: 6_200_000_000,
            budget: 1,
        })
        .user_message();
        assert!(msg.contains("6.2 GB"), "{msg}");
        assert!(!ArchiveOpenError::PasswordRequired.user_message().is_empty());
    }
}
