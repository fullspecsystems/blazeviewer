//! Bounded, RAM-only compressed read-ahead for playback inputs (task #133).
//!
//! SMB delivers a film's *average* bitrate easily but stalls on latency spikes;
//! with a 32 KiB AVIO buffer and demand-driven reads, a spike longer than the
//! decoded-frame queue starves the session (`rebuffers`/`dropped`). This module
//! is the standard player answer: a filler thread reads **compressed bytes**
//! ahead of the demuxer into a budgeted ring, so demux reads hit RAM and the
//! spike is absorbed by buffered depth (64 MiB ≈ 8–10 s of a heavy UHD stream).
//!
//! Privacy: per-file, RAM-only, dropped with the input — the same category as
//! the prefetch ring, not an on-disk cache (Second Directive holds).
//!
//! Concurrency shape (plan #133 slice 2): ONE mutex + two condvars; every wait
//! is time-boxed (25 ms) so a logic bug degrades to polling, never a deadlock.
//! Source reads happen **outside** the lock, tagged with the fill epoch; a
//! reposition (seek outside the window) bumps the epoch, so a stale in-flight
//! read can never land bytes/EOF/errors in the new window (Codex disposition 5).
//! The filler is detached — teardown signals shutdown and never joins, so a
//! read blocked on a dead server can't hang the caller (the `Arc` keeps state
//! alive; that exposure is identical to today's direct blocked read).

use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::read_stats::ReadStats;

/// Default per-ring budget. Two rings play at once (video + audio inputs), so
/// the steady-state ceiling is ~128 MiB — "spend the hardware, inside budgets".
const DEFAULT_CAP: usize = 64 * 1024 * 1024;
/// One filler source read. Small enough that a healthy source bounds how long a
/// stale/teardown read can linger; large enough for good SMB throughput.
const FILL_CHUNK: usize = 1024 * 1024;
/// Wait slice for both condvars — the no-deadlock belt.
const WAIT_SLICE: Duration = Duration::from_millis(25);

/// Positioned reads over the underlying file — the seam the tests fake.
pub trait ByteSource: Send + 'static {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
    fn len(&self) -> u64;
}

/// The real source: a `std::fs::File` + its length, positioned reads so the
/// filler needs no seek state.
pub struct FileSource {
    file: std::fs::File,
    len: u64,
}

impl FileSource {
    pub fn open(path: &std::path::Path) -> io::Result<FileSource> {
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        Ok(FileSource { file, len })
    }
}

impl ByteSource for FileSource {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file.read_at(buf, offset)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            self.file.seek_read(buf, offset)
        }
    }

    fn len(&self) -> u64 {
        self.len
    }
}

/// Resolve the per-ring capacity from the `PB_READAHEAD_MB` override and the
/// file length. `None` = the ring is disabled (explicit `0`, or nothing to
/// buffer). Pure so it's unit-testable; the env read lives in [`cap_from_env`].
fn resolve_cap(env: Option<&str>, file_len: u64) -> Option<usize> {
    let cap = match env {
        None => DEFAULT_CAP,
        Some(s) => match s.trim().parse::<u64>() {
            Ok(0) => return None,
            // Clamp [4, 1024] MiB: below 4 the keep-behind maths degenerate,
            // above 1 GiB is a typo, not a budget (Codex disposition 12).
            Ok(mb) => (mb.clamp(4, 1024) as usize) * 1024 * 1024,
            Err(_) => DEFAULT_CAP,
        },
    };
    if file_len == 0 {
        return None;
    }
    // A ring never larger than the file: fixtures cost fixture-sized rings, so
    // the integration tests genuinely exercise this path (Codex disposition 6).
    Some(cap.min(usize::try_from(file_len).unwrap_or(cap)))
}

/// The env half of [`resolve_cap`].
pub fn cap_from_env(file_len: u64) -> Option<usize> {
    let env = std::env::var("PB_READAHEAD_MB").ok();
    resolve_cap(env.as_deref(), file_len)
}

/// How much already-consumed history the window retains for the demuxer's short
/// backward hops (header/cue re-reads). Scales down with small caps so forward
/// room always dominates (Codex disposition 12).
fn keep_behind_for(cap: usize) -> usize {
    (8 * 1024 * 1024).min(cap / 4)
}

/// One consumer read's outcome, mapped by the AVIO callback in `io.rs`.
#[derive(Debug, PartialEq)]
pub enum ReadOutcome {
    Data(usize),
    Eof,
    Cancelled,
    Failed(String),
}

struct State {
    /// Ring storage; logical window `[win_off, win_off + valid)` of the file,
    /// with the byte at `win_off` stored at `buf[head]`.
    buf: Vec<u8>,
    head: usize,
    valid: usize,
    win_off: u64,
    /// Consumer position (absolute file offset of the next byte to serve).
    pos: u64,
    /// Bumped by every reposition; the filler tags each source read with the
    /// epoch it started under and discards stale results.
    epoch: u64,
    /// Sticky source error — surfaced once the consumer drains buffered bytes.
    err: Option<String>,
    /// The filler reached the end of the source for the current window.
    eof: bool,
    shutdown: bool,
}

struct Shared {
    state: Mutex<State>,
    /// Filler → consumer: bytes appended / EOF / error / shutdown.
    data_ready: Condvar,
    /// Consumer → filler: space reclaimed / reposition / shutdown.
    space_ready: Condvar,
    cap: usize,
    keep_behind: usize,
    len: u64,
}

/// The single consumer handle; drop signals the filler to exit.
pub struct Ring {
    shared: Arc<Shared>,
}

impl Ring {
    /// Spawn the filler and return the consumer handle. `diag` prints the
    /// `src read diag` window line (`PB_VIDEO_DIAG`) — the *true* source-latency
    /// signal, measured where the I/O happens.
    pub fn start(mut src: Box<dyn ByteSource>, cap: usize, diag: bool) -> io::Result<Ring> {
        let len = src.len();
        let cap = cap
            .max(2 * 4096)
            .min(usize::try_from(len).unwrap_or(cap).max(2 * 4096));
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                buf: vec![0; cap],
                head: 0,
                valid: 0,
                win_off: 0,
                pos: 0,
                epoch: 0,
                err: None,
                eof: false,
                shutdown: false,
            }),
            data_ready: Condvar::new(),
            space_ready: Condvar::new(),
            cap,
            keep_behind: keep_behind_for(cap),
            len,
        });
        let filler_shared = shared.clone();
        // Spawn failure propagates — the caller falls back to the direct path
        // rather than failing playback (plan slice 2).
        std::thread::Builder::new()
            .name("pb-readahead".into())
            .spawn(move || filler_loop(&filler_shared, src.as_mut(), diag))?;
        Ok(Ring { shared })
    }

    pub fn len(&self) -> u64 {
        self.shared.len
    }

    pub fn pos(&self) -> u64 {
        self.shared.state.lock().expect("ring lock").pos
    }

    /// Serve up to `out.len()` bytes at the current position, blocking (in
    /// 25 ms slices) while the filler catches up. `abort` is polled at entry
    /// and every slice — a cancel is honored even when data is plentiful
    /// (Codex disposition 9).
    pub fn read(&self, out: &mut [u8], abort: &mut dyn FnMut() -> bool) -> ReadOutcome {
        if out.is_empty() {
            return ReadOutcome::Data(0);
        }
        let sh = &*self.shared;
        let mut st = sh.state.lock().expect("ring lock");
        loop {
            if abort() {
                return ReadOutcome::Cancelled;
            }
            if st.shutdown {
                return ReadOutcome::Failed("readahead torn down".into());
            }
            let win_end = st.win_off + st.valid as u64;
            if st.pos >= st.win_off && st.pos < win_end {
                let n = ((win_end - st.pos) as usize).min(out.len());
                copy_out(&st, st.pos, &mut out[..n]);
                st.pos += n as u64;
                sh.space_ready.notify_all();
                return ReadOutcome::Data(n);
            }
            if st.pos >= sh.len {
                return ReadOutcome::Eof;
            }
            if st.pos < st.win_off || st.pos > win_end {
                // Outside the window (a seek landed here): restart it at `pos`.
                let target = st.pos;
                reposition(&mut st, target);
                sh.space_ready.notify_all();
            } else if let Some(e) = &st.err {
                // pos == fill frontier and the filler died there: surface it.
                return ReadOutcome::Failed(e.clone());
            }
            let (guard, _timeout) = sh
                .data_ready
                .wait_timeout(st, WAIT_SLICE)
                .expect("ring lock");
            st = guard;
        }
    }

    /// Position for the next [`read`](Self::read). In `[0, len]`; `len` is the
    /// legal EOF position. Returns `false` (and moves nothing) out of range.
    pub fn seek(&self, target: u64) -> bool {
        if target > self.shared.len {
            return false;
        }
        let mut st = self.shared.state.lock().expect("ring lock");
        st.pos = target;
        let win_end = st.win_off + st.valid as u64;
        if target < st.win_off || target > win_end {
            reposition(&mut st, target);
            self.shared.space_ready.notify_all();
        }
        true
    }

    /// Test/diag hook: `(win_off, window_valid_bytes, capacity)`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn window(&self) -> (u64, usize, usize) {
        let st = self.shared.state.lock().expect("ring lock");
        (st.win_off, st.valid, self.shared.cap)
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        let mut st = self.shared.state.lock().expect("ring lock");
        st.shutdown = true;
        self.shared.data_ready.notify_all();
        self.shared.space_ready.notify_all();
    }
}

/// Restart the window at `target` and invalidate any in-flight fill.
fn reposition(st: &mut State, target: u64) {
    st.win_off = target;
    st.head = 0;
    st.valid = 0;
    st.eof = false;
    st.err = None;
    st.epoch += 1;
}

/// Copy `out.len()` bytes at absolute offset `at` (must be inside the window)
/// out of the ring, handling wrap.
fn copy_out(st: &State, at: u64, out: &mut [u8]) {
    let cap = st.buf.len();
    let start = (st.head + (at - st.win_off) as usize) % cap;
    let first = out.len().min(cap - start);
    out[..first].copy_from_slice(&st.buf[start..start + first]);
    if first < out.len() {
        let rest = out.len() - first;
        out[first..].copy_from_slice(&st.buf[..rest]);
    }
}

/// Append `data` at the fill frontier (`win_off + valid`), handling wrap.
/// Caller guarantees `valid + data.len() <= cap`.
fn append(st: &mut State, data: &[u8]) {
    let cap = st.buf.len();
    let start = (st.head + st.valid) % cap;
    let first = data.len().min(cap - start);
    st.buf[start..start + first].copy_from_slice(&data[..first]);
    if first < data.len() {
        let rest = data.len() - first;
        st.buf[..rest].copy_from_slice(&data[first..]);
    }
    st.valid += data.len();
}

/// What the filler decided to do next, under the lock.
enum FillPlan {
    Read { epoch: u64, off: u64, want: usize },
    Exit,
}

/// Pick the next fill under the lock: reclaim consumed history beyond
/// `keep_behind` when full; wait (time-boxed) when there's nothing to do.
fn plan_fill(sh: &Shared) -> FillPlan {
    let mut st = sh.state.lock().expect("ring lock");
    loop {
        if st.shutdown {
            return FillPlan::Exit;
        }
        // A sticky error ends the filler; a later reposition clears it and the
        // consumer's next out-of-window read... cannot restart a dead thread —
        // so the filler ONLY exits on shutdown; on error it waits for a
        // reposition (which clears `err`) and tries again. Transient network
        // errors thus retry on the next seek instead of killing playback.
        if st.err.is_some() {
            let (guard, _t) = sh.space_ready.wait_timeout(st, WAIT_SLICE).expect("lock");
            st = guard;
            continue;
        }
        let fill_at = st.win_off + st.valid as u64;
        if st.eof || fill_at >= sh.len {
            if !st.eof {
                st.eof = true;
                sh.data_ready.notify_all();
            }
            let (guard, _t) = sh.space_ready.wait_timeout(st, WAIT_SLICE).expect("lock");
            st = guard;
            continue;
        }
        if st.valid == sh.cap {
            // Full: drop history the consumer is done with (keep `keep_behind`
            // for short back-hops).
            let reclaim_to = st.pos.saturating_sub(sh.keep_behind as u64);
            let reclaimable = reclaim_to.saturating_sub(st.win_off) as usize;
            if reclaimable == 0 {
                let (guard, _t) = sh.space_ready.wait_timeout(st, WAIT_SLICE).expect("lock");
                st = guard;
                continue;
            }
            st.head = (st.head + reclaimable) % sh.cap;
            st.win_off += reclaimable as u64;
            st.valid -= reclaimable;
        }
        let want = (sh.cap - st.valid)
            .min(FILL_CHUNK)
            .min((sh.len - fill_at) as usize);
        return FillPlan::Read {
            epoch: st.epoch,
            off: fill_at,
            want,
        };
    }
}

fn filler_loop(sh: &Shared, src: &mut dyn ByteSource, diag: bool) {
    let mut scratch = vec![0u8; FILL_CHUNK.min(sh.cap)];
    let mut stats = diag.then(|| ReadStats::new(Instant::now()));
    loop {
        let (epoch, off, want) = match plan_fill(sh) {
            FillPlan::Exit => return,
            FillPlan::Read { epoch, off, want } => (epoch, off, want),
        };
        // The source read runs OUTSIDE the lock — the consumer keeps draining.
        let t0 = Instant::now();
        let result = src.read_at(off, &mut scratch[..want]);
        if let Some(stats) = stats.as_mut() {
            let now = Instant::now();
            let bytes = *result.as_ref().unwrap_or(&0) as u64;
            if let Some(w) = stats.fold_bytes(now - t0, bytes, now) {
                let (valid, cap) = {
                    let st = sh.state.lock().expect("ring lock");
                    (st.valid, sh.cap)
                };
                eprintln!(
                    "[pb-video] src read diag: {:.1}s — {} reads, avg {:.1}ms max {:.1}ms, >20ms={} >40ms={}, {:.1} MB/s, window {}%",
                    w.secs,
                    w.reads,
                    w.avg_ms,
                    w.max_ms,
                    w.over_20,
                    w.over_40,
                    w.bytes as f64 / w.secs / 1e6,
                    valid * 100 / cap.max(1),
                );
            }
        }
        let mut st = sh.state.lock().expect("ring lock");
        if st.shutdown {
            return;
        }
        if st.epoch != epoch {
            continue; // superseded by a reposition — stale bytes discarded
        }
        match result {
            // Short reads before EOF are normal; only 0 at the frontier means
            // the source ended early (treat as EOF at the shrunken length).
            Ok(0) => {
                st.eof = true;
                sh.data_ready.notify_all();
            }
            Ok(n) => {
                append(&mut st, &scratch[..n]);
                sh.data_ready.notify_all();
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                st.err = Some(format!("readahead source: {e}"));
                sh.data_ready.notify_all();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    /// A deterministic in-RAM source: byte at offset `i` is `hash(i)`; counts
    /// reads; optional artificial latency and a one-shot error window.
    struct FakeSource {
        len: u64,
        reads: Arc<AtomicU32>,
        delay: Duration,
        /// Return an error for reads starting inside this range, once armed.
        fail_at: Option<u64>,
        /// Cap each read to this many bytes (short-read behavior).
        short: Option<usize>,
        /// Block until cleared (for stale-fill orchestration).
        gate: Option<Arc<AtomicBool>>,
    }

    impl FakeSource {
        fn new(len: u64) -> FakeSource {
            FakeSource {
                len,
                reads: Arc::new(AtomicU32::new(0)),
                delay: Duration::ZERO,
                fail_at: None,
                short: None,
                gate: None,
            }
        }
    }

    fn byte_at(i: u64) -> u8 {
        (i.wrapping_mul(31).wrapping_add(i >> 8)) as u8
    }

    impl ByteSource for FakeSource {
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if let Some(g) = &self.gate {
                while g.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            std::thread::sleep(self.delay);
            let remaining = self.len.saturating_sub(offset);
            if remaining == 0 {
                return Ok(0);
            }
            let mut n = buf.len().min(remaining as usize);
            if let Some(at) = self.fail_at {
                if offset >= at {
                    return Err(io::Error::other("injected failure"));
                }
                // A read straddling the bad range short-reads up to it, so the
                // NEXT read starts there and errors — like a real bad region.
                n = n.min((at - offset) as usize);
            }
            if let Some(s) = self.short {
                n = n.min(s);
            }
            for (k, b) in buf[..n].iter_mut().enumerate() {
                *b = byte_at(offset + k as u64);
            }
            Ok(n)
        }

        fn len(&self) -> u64 {
            self.len
        }
    }

    fn no_abort() -> impl FnMut() -> bool {
        || false
    }

    /// Read exactly `n` bytes (looping over partial serves) or panic.
    fn read_exact(ring: &Ring, n: usize) -> Vec<u8> {
        let mut out = vec![0u8; n];
        let mut got = 0;
        while got < n {
            match ring.read(&mut out[got..], &mut no_abort()) {
                ReadOutcome::Data(k) => got += k,
                other => panic!("expected data, got {other:?} after {got}/{n}"),
            }
        }
        out
    }

    fn expected(start: u64, n: usize) -> Vec<u8> {
        (0..n as u64).map(|k| byte_at(start + k)).collect()
    }

    /// Tiny capacity vs a larger file forces wrap + reclaim continuously.
    #[test]
    fn sequential_reads_across_wrap_return_source_bytes() {
        let src = FakeSource::new(8_000_000);
        let ring = Ring::start(Box::new(src), 64 * 1024, false).unwrap();
        let mut pos = 0u64;
        // Uneven read sizes so serves straddle the wrap point repeatedly.
        for (i, sz) in [4096usize, 33_000, 1, 8191, 100_000, 60_000]
            .into_iter()
            .cycle()
            .take(40)
            .enumerate()
        {
            let got = read_exact(&ring, sz);
            assert_eq!(got, expected(pos, sz), "read {i} at {pos} x{sz}");
            pos += sz as u64;
        }
    }

    #[test]
    fn read_blocks_on_slow_source_then_completes() {
        let mut src = FakeSource::new(256 * 1024);
        src.delay = Duration::from_millis(30);
        let ring = Ring::start(Box::new(src), 64 * 1024, false).unwrap();
        assert_eq!(read_exact(&ring, 200_000), expected(0, 200_000));
    }

    #[test]
    fn short_backward_seek_is_served_from_keep_behind_without_refetch() {
        let src = FakeSource::new(4 * 1024 * 1024);
        let reads = src.reads.clone();
        let ring = Ring::start(Box::new(src), 1024 * 1024, false).unwrap();
        let _ = read_exact(&ring, 700_000);
        // Wait until the filler is provably parked: window full AND nothing
        // left to reclaim (win_off has advanced to pos − keep_behind). Full
        // alone isn't quiescent — the filler reclaims lazily and would take
        // one more source read after the snapshot.
        let keep = keep_behind_for(1024 * 1024) as u64;
        let t0 = Instant::now();
        loop {
            let (win_off, valid, cap) = ring.window();
            if valid == cap && win_off >= 700_000 - keep {
                break;
            }
            assert!(t0.elapsed() < Duration::from_secs(5), "filler never parked");
            std::thread::sleep(Duration::from_millis(2));
        }
        let before = reads.load(Ordering::SeqCst);
        // keep_behind = cap/4 = 256 KiB; hop 100 KiB back — well within it.
        assert!(ring.seek(600_000));
        let got = read_exact(&ring, 50_000);
        assert_eq!(got, expected(600_000, 50_000));
        assert_eq!(
            reads.load(Ordering::SeqCst),
            before,
            "history hop must not re-read the source"
        );
    }

    #[test]
    fn far_forward_seek_repositions_and_serves_the_target() {
        let src = FakeSource::new(8 * 1024 * 1024);
        let ring = Ring::start(Box::new(src), 256 * 1024, false).unwrap();
        let _ = read_exact(&ring, 10_000);
        assert!(ring.seek(6_000_000));
        assert_eq!(read_exact(&ring, 40_000), expected(6_000_000, 40_000));
    }

    /// The epoch protocol: a fill in flight when a reposition lands must be
    /// discarded, not appended into the new window (Codex disposition 5).
    #[test]
    fn superseding_reposition_discards_a_stale_in_flight_fill() {
        let mut src = FakeSource::new(8 * 1024 * 1024);
        let gate = Arc::new(AtomicBool::new(true));
        src.gate = Some(gate.clone());
        let ring = Ring::start(Box::new(src), 256 * 1024, false).unwrap();
        // The filler is now blocked inside read_at(0) holding NO lock. Seek far
        // away: the epoch bumps, the stale fill's bytes must vanish.
        std::thread::sleep(Duration::from_millis(20));
        assert!(ring.seek(4_000_000));
        gate.store(false, Ordering::SeqCst); // release the stale read
        let got = read_exact(&ring, 64 * 1024);
        assert_eq!(got, expected(4_000_000, 64 * 1024), "no stale bytes at 0");
    }

    #[test]
    fn cancel_unblocks_a_waiting_read_promptly() {
        let mut src = FakeSource::new(1024 * 1024);
        src.delay = Duration::from_secs(5); // effectively never fills in time
        let ring = Ring::start(Box::new(src), 64 * 1024, false).unwrap();
        // The read blocks (nothing buffered yet); the abort flips ~80 ms in.
        // The wait loop polls in 25 ms slices, so it must return well before
        // the 5 s source read would have produced data.
        let t0 = Instant::now();
        let mut buf = [0u8; 4096];
        let out = ring.read(&mut buf, &mut || t0.elapsed() > Duration::from_millis(80));
        assert_eq!(out, ReadOutcome::Cancelled);
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "cancel must not wait out the source"
        );
    }

    #[test]
    fn cancel_is_honored_at_entry_even_with_data_available() {
        let src = FakeSource::new(64 * 1024);
        let ring = Ring::start(Box::new(src), 64 * 1024, false).unwrap();
        let _ = read_exact(&ring, 1); // window has data
        let mut buf = [0u8; 16];
        assert_eq!(ring.read(&mut buf, &mut || true), ReadOutcome::Cancelled);
    }

    #[test]
    fn source_error_surfaces_when_reached_and_history_still_serves() {
        let mut src = FakeSource::new(1024 * 1024);
        src.fail_at = Some(128 * 1024);
        let ring = Ring::start(Box::new(src), 64 * 1024, false).unwrap();
        // Everything before the failure offset reads fine…
        let got = read_exact(&ring, 100_000);
        assert_eq!(got, expected(0, 100_000));
        // …and at the frontier the error surfaces instead of hanging.
        let mut rest = vec![0u8; 128 * 1024];
        let mut served = 100_000usize;
        loop {
            match ring.read(&mut rest, &mut no_abort()) {
                ReadOutcome::Data(n) => served += n,
                ReadOutcome::Failed(e) => {
                    assert!(e.contains("injected failure"), "got: {e}");
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
            assert!(served <= 128 * 1024, "must not serve past the failure");
        }
    }

    #[test]
    fn short_source_reads_are_transparent() {
        let mut src = FakeSource::new(300_000);
        src.short = Some(1000); // source never returns more than 1000 bytes
        let ring = Ring::start(Box::new(src), 64 * 1024, false).unwrap();
        assert_eq!(read_exact(&ring, 300_000), expected(0, 300_000));
        let mut buf = [0u8; 1];
        assert_eq!(ring.read(&mut buf, &mut no_abort()), ReadOutcome::Eof);
    }

    #[test]
    fn eof_and_seek_bounds() {
        let src = FakeSource::new(10_000);
        let ring = Ring::start(Box::new(src), 8 * 1024, false).unwrap();
        assert!(ring.seek(10_000), "len is the legal EOF position");
        let mut buf = [0u8; 1];
        assert_eq!(ring.read(&mut buf, &mut no_abort()), ReadOutcome::Eof);
        assert!(!ring.seek(10_001), "past-end seek is rejected");
        assert!(ring.seek(0));
        assert_eq!(read_exact(&ring, 10_000), expected(0, 10_000));
    }

    /// The window never exceeds capacity under sustained churn.
    #[test]
    fn window_never_exceeds_capacity() {
        let src = FakeSource::new(2 * 1024 * 1024);
        let ring = Ring::start(Box::new(src), 100_000, false).unwrap();
        let mut pos = 0u64;
        while pos < 1_900_000 {
            let _ = read_exact(&ring, 37_000);
            pos += 37_000;
            let (_, valid, cap) = ring.window();
            assert!(valid <= cap, "window {valid} exceeds capacity {cap}");
        }
    }

    #[test]
    fn cap_resolution_rules() {
        let mib = 1024 * 1024;
        assert_eq!(resolve_cap(None, u64::MAX), Some(64 * mib), "default");
        assert_eq!(resolve_cap(Some("0"), u64::MAX), None, "0 disables");
        assert_eq!(resolve_cap(Some("16"), u64::MAX), Some(16 * mib));
        assert_eq!(resolve_cap(Some("1"), u64::MAX), Some(4 * mib), "clamp low");
        assert_eq!(
            resolve_cap(Some("9999"), u64::MAX),
            Some(1024 * mib),
            "clamp high"
        );
        assert_eq!(resolve_cap(Some("junk"), u64::MAX), Some(64 * mib));
        assert_eq!(
            resolve_cap(None, 200 * 1024),
            Some(200 * 1024),
            "ring never larger than the file"
        );
        assert_eq!(resolve_cap(None, 0), None, "nothing to buffer");
    }

    #[test]
    fn keep_behind_scales_with_small_caps() {
        assert_eq!(keep_behind_for(64 * 1024 * 1024), 8 * 1024 * 1024);
        assert_eq!(keep_behind_for(4 * 1024 * 1024), 1024 * 1024);
        assert_eq!(keep_behind_for(100_000), 25_000);
    }
}
