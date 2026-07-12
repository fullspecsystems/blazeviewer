//! Priority decode worker pool (plan §3.2 / §3.4; multi-consumer contract task #83).
//!
//! Decode + file I/O run here, **never on the event loop**. The pool pulls the
//! highest-priority job (priority = want-list index, 0 = the on-screen image),
//! reads the bytes off disk, decodes-to-fit, and ships the result back over a
//! channel for the main thread to upload during prefetch.
//!
//! Properties that make it safe under fast navigation:
//! - **Priority + dedup:** the current image jumps the queue; an item already
//!   queued or in-flight *for the same purpose* is never decoded twice.
//! - **Cancellation:** `set_targets` flags jobs no longer wanted; queued ones are
//!   dropped and an in-flight one's result is discarded when it finishes.
//! - **Byte-budget backpressure:** workers park rather than decode further ahead
//!   than the uploader can drain, so memory stays bounded no matter how deep the
//!   prefetch window is (worker count is capped too — see `recommended_workers`).
//! - **Multi-consumer identity (task #83):** a request is `(item, purpose)` within
//!   an epoch, so a thumbnail want for item N can never cancel, dedup away, or be
//!   deduped away by the viewer's display want for the same N. The caller composes
//!   ONE merged priority list with every display/poster want ahead of every thumb
//!   want; the pool trusts that order.
//! - **Occupancy guard (task #83):** priority alone can't prevent inversion — if
//!   every worker were mid-thumb-decode when a nav keypress arrived, the display
//!   job would wait behind them. Thumb-purpose jobs are capped at
//!   `max(1, workers - 2)` concurrent, so a display job always finds a free worker.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use pb_decode::{DecodeError, DecodedImage, FitBox};
use pb_source::PhotoSource;

/// Which consumer a decode serves. `Display` covers the viewer's whole ladder
/// (current / previews / sharp-ring fulls / video posters — their relative
/// priority is the want-list *order*); `Thumb` is the Thumbnails strip (task
/// #83). Separating consumers in the request identity is what guarantees thumb
/// demand can never displace display work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Purpose {
    Display,
    Thumb,
}

/// The injected decode step (resolves the item's bytes from the source, then
/// decodes; a fake in tests). The pool is **source-agnostic** — it carries the
/// `PhotoSource` and an item index, never a path, so a filesystem listing and a
/// ZIP archive flow through the same pool. The `bool` is `allow_preview`: true
/// requests a fast embedded preview where one exists (HEIC thumbnail, RAW
/// preview), false forces the full-resolution decode. The [`Purpose`] lets the
/// decode route format-specific fast paths (a thumb decode may use the EXIF
/// IFD1 thumbnail; a display decode never does). The `&AtomicBool` is the
/// job's cancel flag, set by `set_targets` when the item is no longer wanted —
/// long steps (the video poster walk, task #79) check it mid-job; single-shot
/// image decodes may ignore it (the result is discarded either way).
pub type DecodeFn = dyn Fn(
        &dyn PhotoSource,
        usize,
        Option<FitBox>,
        bool,
        Purpose,
        &AtomicBool,
    ) -> Result<DecodedImage, DecodeError>
    + Send
    + Sync;

/// Identifies a unit of decode work: which item, for which consumer, at which
/// geometry epoch. The epoch rides back on the [`Outcome`] so the main thread
/// can discard a result decoded for a stale geometry (after a resize / fit
/// toggle); the purpose routes the result to its consumer (ring vs thumb cache).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DecodeKey {
    pub item: usize,
    pub epoch: u64,
    pub purpose: Purpose,
}

/// One prioritized want (the `set_targets` element): highest priority first in
/// the slice. The caller (AppCore's `request_prefetch`) composes display wants
/// ahead of thumb wants in a single merged list.
#[derive(Debug, Clone, Copy)]
pub struct Want {
    pub item: usize,
    pub fit: Option<FitBox>,
    pub preview: bool,
    pub purpose: Purpose,
}

impl Want {
    pub fn display(item: usize, fit: Option<FitBox>, preview: bool) -> Want {
        Want {
            item,
            fit,
            preview,
            purpose: Purpose::Display,
        }
    }

    /// A thumbnail fill: always preview-friendly (embedded previews are the
    /// cheap tier), fitted to the thumb box.
    pub fn thumb(item: usize, fit: FitBox) -> Want {
        Want {
            item,
            fit: Some(fit),
            preview: true,
            purpose: Purpose::Thumb,
        }
    }
}

/// A finished decode handed back to the main thread. Dropping it frees the item's
/// bytes from the pool's in-flight budget (RAII), so the upload→drop cycle is what
/// lets workers proceed.
pub struct Outcome {
    pub key: DecodeKey,
    pub result: Result<DecodedImage, DecodeError>,
    /// The pool's byte-budget RAII (freed on drop). `None` for a *synthetic* outcome not
    /// produced by the pool — a macOS archive-video poster the shell generated and fed back
    /// in (`Outcome::synthetic`); it carries no pool budget.
    _budget: Option<BudgetGuard>,
}

impl Outcome {
    /// A result that did **not** come from the decode pool — the macOS shell's archive-video
    /// poster (generated via `AVAssetImageGenerator`), fed into the resident ring through the
    /// normal `drain_results` upload path. Carries no pool byte-budget.
    pub fn synthetic(item: usize, epoch: u64, result: Result<DecodedImage, DecodeError>) -> Self {
        Outcome {
            key: DecodeKey {
                item,
                epoch,
                purpose: Purpose::Display,
            },
            result,
            _budget: None,
        }
    }

    /// Move the decoded image out, dropping the pool byte-budget reservation —
    /// the T0 handoff (task #83): after the ring upload, the CPU buffer travels
    /// to the thumb-derive thread without cloning, and the budget frees here so
    /// the derive thread can never backpressure display decodes.
    pub fn into_image(self) -> Option<DecodedImage> {
        self.result.ok()
    }
}

struct BudgetGuard {
    shared: Arc<Shared>,
    bytes: usize,
}

impl Drop for BudgetGuard {
    fn drop(&mut self) {
        if self.bytes == 0 {
            return;
        }
        let mut inner = self.shared.inner.lock().unwrap();
        inner.inflight_bytes = inner.inflight_bytes.saturating_sub(self.bytes);
        drop(inner);
        self.shared.cv.notify_all();
    }
}

struct Job {
    key: DecodeKey,
    /// The source to resolve this item's bytes from. Carried per-job (not stored
    /// once on the pool) so a playlist rebuild — which hands a new source to the
    /// next `set_targets` — can't make an in-flight decode resolve against the
    /// wrong source; the stale job keeps its original source and its result is
    /// discarded by epoch/want checks.
    source: Arc<dyn PhotoSource>,
    fit: Option<FitBox>,
    /// Whether to decode a fast preview (true) or the full resolution (false).
    preview: bool,
    prio: u32,
    cancel: Arc<AtomicBool>,
}

struct Inner {
    queue: Vec<Job>,
    /// (item, purpose) -> cancel flag, for every queued OR in-flight job (the
    /// dedup set). Purpose-keyed so consumers can't cancel each other (task #83).
    tracked: HashMap<(usize, Purpose), Arc<AtomicBool>>,
    /// Decoded-but-not-yet-drained bytes (the backpressure counter).
    inflight_bytes: usize,
    /// Thumb-purpose jobs currently decoding (the occupancy guard's counter).
    thumb_inflight: usize,
    epoch: u64,
    shutdown: bool,
}

struct Shared {
    inner: Mutex<Inner>,
    cv: Condvar,
    decode: Arc<DecodeFn>,
    results_tx: Sender<Outcome>,
    byte_budget: usize,
    /// Max concurrent thumb-purpose decodes: `max(1, workers - 2)`, so display
    /// jobs always find a free worker (the anti-inversion guard, task #83).
    thumb_cap: usize,
}

/// A capped worker count: leave a core for the event loop, but never spin up the
/// dozens a 16–32 core box would otherwise (each worker holds a full decode +
/// resize buffer). 2–8 is the measured sweet spot.
pub fn recommended_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(1))
        .unwrap_or(4)
        .clamp(2, 8)
}

pub struct DecodePool {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
}

impl DecodePool {
    /// Spawn `workers` threads decoding via `decode`. `byte_budget` caps decoded-
    /// but-undrained bytes. Returns the pool and the outcome receiver.
    pub fn new(
        workers: usize,
        byte_budget: usize,
        decode: Arc<DecodeFn>,
    ) -> (Self, Receiver<Outcome>) {
        let workers = workers.max(1);
        let (results_tx, results_rx) = channel();
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                queue: Vec::new(),
                tracked: HashMap::new(),
                inflight_bytes: 0,
                thumb_inflight: 0,
                epoch: 0,
                shutdown: false,
            }),
            cv: Condvar::new(),
            decode,
            results_tx,
            byte_budget: byte_budget.max(1),
            thumb_cap: workers.saturating_sub(2).max(1),
        });
        let workers = (0..workers)
            .map(|_| {
                let shared = shared.clone();
                std::thread::spawn(move || worker_loop(shared))
            })
            .collect();
        (Self { shared, workers }, results_rx)
    }

    /// Replace the want-set with `prioritized` (highest priority first), at
    /// `epoch`. Cancels jobs no longer wanted, re-prioritizes queued ones, and
    /// enqueues newly-wanted items. An epoch change cancels everything stale.
    /// Identity is `(item, purpose)`: a thumb want and a display want for the
    /// same item coexist, and dropping one never cancels the other.
    pub fn set_targets(&self, epoch: u64, source: &Arc<dyn PhotoSource>, prioritized: &[Want]) {
        let mut inner = self.shared.inner.lock().unwrap();

        if epoch != inner.epoch {
            // Geometry changed: every queued/in-flight job is for the old size.
            for flag in inner.tracked.values() {
                flag.store(true, Ordering::Release);
            }
            inner.queue.clear();
            inner.tracked.clear();
            inner.epoch = epoch;
        }

        let wanted: HashMap<(usize, Purpose), u32> = prioritized
            .iter()
            .enumerate()
            .map(|(i, w)| ((w.item, w.purpose), i as u32))
            .collect();

        // Cancel anything no longer wanted; drop those still queued.
        for (key, flag) in inner.tracked.iter() {
            if !wanted.contains_key(key) {
                flag.store(true, Ordering::Release);
            }
        }
        inner
            .queue
            .retain(|j| wanted.contains_key(&(j.key.item, j.key.purpose)));
        let live: std::collections::HashSet<(usize, Purpose)> = inner
            .queue
            .iter()
            .map(|j| (j.key.item, j.key.purpose))
            .collect();
        inner.tracked.retain(|key, flag| {
            wanted.contains_key(key) && (live.contains(key) || !flag.load(Ordering::Acquire))
        });

        // Re-prioritize jobs still queued.
        for job in inner.queue.iter_mut() {
            if let Some(&prio) = wanted.get(&(job.key.item, job.key.purpose)) {
                job.prio = prio;
            }
        }

        // Enqueue newly-wanted items (dedup against queued + in-flight).
        for w in prioritized {
            let key = (w.item, w.purpose);
            if inner.tracked.contains_key(&key) {
                continue;
            }
            let flag = Arc::new(AtomicBool::new(false));
            inner.tracked.insert(key, flag.clone());
            let prio = wanted[&key];
            inner.queue.push(Job {
                key: DecodeKey {
                    item: w.item,
                    epoch,
                    purpose: w.purpose,
                },
                source: source.clone(),
                fit: w.fit,
                preview: w.preview,
                prio,
                cancel: flag,
            });
        }

        drop(inner);
        self.shared.cv.notify_all();
    }
}

impl Drop for DecodePool {
    fn drop(&mut self) {
        {
            let mut inner = self.shared.inner.lock().unwrap();
            inner.shutdown = true;
        }
        self.shared.cv.notify_all();
        for h in self.workers.drain(..) {
            let _ = h.join();
        }
    }
}

fn worker_loop(shared: Arc<Shared>) {
    loop {
        // Wait for a runnable job: one exists, we're under the byte budget, and —
        // for thumb jobs — under the thumb occupancy cap.
        let job = {
            let mut inner = shared.inner.lock().unwrap();
            loop {
                if inner.shutdown {
                    return;
                }
                if inner.inflight_bytes < shared.byte_budget {
                    if let Some(job) = pop_best(&mut inner, shared.thumb_cap) {
                        if job.key.purpose == Purpose::Thumb {
                            inner.thumb_inflight += 1;
                        }
                        break job;
                    }
                }
                inner = shared.cv.wait(inner).unwrap();
            }
        };
        let is_thumb = job.key.purpose == Purpose::Thumb;

        // Cancelled before it ran: forget it and move on.
        if job.cancel.load(Ordering::Acquire) {
            let mut inner = shared.inner.lock().unwrap();
            untrack(&mut inner, &job);
            release_thumb_slot(&mut inner, is_thumb);
            drop(inner);
            shared.cv.notify_all();
            continue;
        }

        let result = (shared.decode)(
            job.source.as_ref(),
            job.key.item,
            job.fit,
            job.preview,
            job.key.purpose,
            &job.cancel,
        );
        let bytes = match &result {
            Ok(img) => img.pixels.len(),
            Err(_) => 0,
        };

        // Account for the result and stop tracking the item — unless it was
        // cancelled mid-decode, in which case discard the result entirely.
        {
            let mut inner = shared.inner.lock().unwrap();
            untrack(&mut inner, &job);
            release_thumb_slot(&mut inner, is_thumb);
            if job.cancel.load(Ordering::Acquire) {
                drop(inner);
                shared.cv.notify_all();
                continue;
            }
            inner.inflight_bytes += bytes;
        }
        // A freed thumb slot may unblock a parked worker even while the byte
        // budget is unchanged.
        if is_thumb {
            shared.cv.notify_all();
        }

        let outcome = Outcome {
            key: job.key,
            result,
            _budget: Some(BudgetGuard {
                shared: shared.clone(),
                bytes,
            }),
        };
        if shared.results_tx.send(outcome).is_err() {
            return; // receiver gone; the guard frees the bytes as it drops
        }
    }
}

fn release_thumb_slot(inner: &mut Inner, is_thumb: bool) {
    if is_thumb {
        inner.thumb_inflight = inner.thumb_inflight.saturating_sub(1);
    }
}

/// Remove the dedup entry for the job's `(item, purpose)` only if it still maps
/// to *this* job's flag. A newer job for the same key (e.g. re-requested after
/// an epoch change while this one was cancelled mid-decode) must keep its own
/// entry so it can still be deduped and cancelled.
fn untrack(inner: &mut Inner, job: &Job) {
    let key = (job.key.item, job.key.purpose);
    if inner
        .tracked
        .get(&key)
        .is_some_and(|f| Arc::ptr_eq(f, &job.cancel))
    {
        inner.tracked.remove(&key);
    }
}

/// Remove and return the highest-priority (lowest `prio`) *runnable* job: thumb
/// jobs are skipped while the occupancy cap is reached, so a display job behind
/// a queue of thumbs still runs immediately.
fn pop_best(inner: &mut Inner, thumb_cap: usize) -> Option<Job> {
    let thumbs_blocked = inner.thumb_inflight >= thumb_cap;
    let idx = inner
        .queue
        .iter()
        .enumerate()
        .filter(|(_, j)| !(thumbs_blocked && j.key.purpose == Purpose::Thumb))
        .min_by_key(|(_, j)| j.prio)
        .map(|(i, _)| i)?;
    Some(inner.queue.swap_remove(idx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pb_decode::PixelFormat;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    /// A stand-in source for the pool tests, which exercise scheduling /
    /// cancellation / budget only — the decode fns key off the item index and
    /// never touch the source's bytes.
    struct FakeSource;
    impl PhotoSource for FakeSource {
        fn len(&self) -> usize {
            usize::MAX
        }
        fn name(&self, _i: usize) -> &str {
            "fake"
        }
        fn bytes(&self, _i: usize) -> std::io::Result<Vec<u8>> {
            Ok(Vec::new())
        }
    }

    fn source() -> Arc<dyn PhotoSource> {
        Arc::new(FakeSource)
    }

    fn image(item: usize, bytes: usize) -> DecodedImage {
        DecodedImage {
            width: 2,
            height: 2,
            orig_width: 2,
            orig_height: 2,
            codec: "test",
            format: PixelFormat::Rgba8,
            pixels: vec![item as u8; bytes],
            is_preview: false,
            color: pb_decode::ColorTransform::srgb(),
            peak: 1.0,
            animated: None,
        }
    }

    fn targets(items: &[usize]) -> Vec<Want> {
        items
            .iter()
            .map(|&i| Want::display(i, None, false))
            .collect()
    }

    fn thumb_box() -> FitBox {
        FitBox {
            max_width: 512,
            max_height: 512,
        }
    }

    fn drain_n(rx: &Receiver<Outcome>, n: usize) -> Vec<usize> {
        let mut got = Vec::new();
        for _ in 0..n {
            let o = rx
                .recv_timeout(Duration::from_secs(5))
                .expect("outcome before timeout");
            got.push(o.key.item);
        }
        got
    }

    #[test]
    fn untrack_only_removes_matching_flag() {
        let mut inner = Inner {
            queue: Vec::new(),
            tracked: HashMap::new(),
            inflight_bytes: 0,
            thumb_inflight: 0,
            epoch: 0,
            shutdown: false,
        };
        let old = Arc::new(AtomicBool::new(false));
        let new = Arc::new(AtomicBool::new(false));
        let job = |flag: &Arc<AtomicBool>| Job {
            key: DecodeKey {
                item: 5,
                epoch: 0,
                purpose: Purpose::Display,
            },
            source: source(),
            fit: None,
            preview: false,
            prio: 0,
            cancel: flag.clone(),
        };
        // A fresh job (re-requested after an epoch change) now owns item 5.
        inner.tracked.insert((5, Purpose::Display), new.clone());
        // The old, cancelled job finishing must NOT drop the new job's entry.
        untrack(&mut inner, &job(&old));
        assert!(inner.tracked.contains_key(&(5, Purpose::Display)));
        // The owning job removes its own entry.
        untrack(&mut inner, &job(&new));
        assert!(!inner.tracked.contains_key(&(5, Purpose::Display)));
    }

    #[test]
    fn delivers_all_wanted_items() {
        let decode: Arc<DecodeFn> = Arc::new(|_s, item, _, _, _, _| Ok(image(item, 16)));
        let (pool, rx) = DecodePool::new(3, 1 << 20, decode);
        let src = source();
        pool.set_targets(1, &src, &targets(&[0, 1, 2, 3, 4]));
        let mut got = drain_n(&rx, 5);
        got.sort();
        assert_eq!(got, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn decodes_in_priority_order_with_one_worker() {
        let order = Arc::new(StdMutex::new(Vec::<usize>::new()));
        let rec = order.clone();
        let decode: Arc<DecodeFn> = Arc::new(move |_s, item, _, _, _, _| {
            rec.lock().unwrap().push(item);
            Ok(image(item, 16))
        });
        let (pool, rx) = DecodePool::new(1, 1 << 20, decode);
        let src = source();
        pool.set_targets(1, &src, &targets(&[0, 1, 2, 3]));
        drain_n(&rx, 4);
        assert_eq!(*order.lock().unwrap(), vec![0, 1, 2, 3]);
    }

    #[test]
    fn cancels_superseded_targets() {
        // The first decode blocks until released, so we can swap targets while an
        // item is in-flight and a batch is queued behind it.
        let (started_tx, started_rx) = channel::<()>();
        let (release_tx, release_rx) = channel::<()>();
        let gate = Arc::new(AtomicBool::new(true)); // true => next decode gates
        let release_rx = StdMutex::new(release_rx);
        let decode: Arc<DecodeFn> = Arc::new(move |_s, item, _, _, _, _| {
            if gate.swap(false, Ordering::SeqCst) {
                started_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
            }
            Ok(image(item, 16))
        });
        let (pool, rx) = DecodePool::new(1, 1 << 20, decode);
        let src = source();
        pool.set_targets(1, &src, &targets(&[0, 1, 2, 3, 4]));
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap(); // item 0 in-flight
                                                                  // Swap to a disjoint set: 0 (in-flight) + 1..4 (queued) are all cancelled.
        pool.set_targets(1, &src, &targets(&[10, 11]));
        release_tx.send(()).unwrap();

        let mut got = drain_n(&rx, 2);
        got.sort();
        assert_eq!(got, vec![10, 11], "only the live targets survive");
        // Nothing else should arrive.
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
    }

    #[test]
    fn does_not_decode_the_same_item_twice() {
        let count = Arc::new(StdMutex::new(0usize));
        let (started_tx, started_rx) = channel::<()>();
        let (release_tx, release_rx) = channel::<()>();
        let gate = Arc::new(AtomicBool::new(true));
        let release_rx = StdMutex::new(release_rx);
        let c = count.clone();
        let decode: Arc<DecodeFn> = Arc::new(move |_s, item, _, _, _, _| {
            *c.lock().unwrap() += 1;
            if gate.swap(false, Ordering::SeqCst) {
                started_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
            }
            Ok(image(item, 16))
        });
        let (pool, rx) = DecodePool::new(1, 1 << 20, decode);
        let src = source();
        pool.set_targets(1, &src, &targets(&[0, 1, 2]));
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // Re-request the same set while 0 is in-flight and 1,2 are queued.
        pool.set_targets(1, &src, &targets(&[0, 1, 2]));
        release_tx.send(()).unwrap();
        drain_n(&rx, 3);
        assert_eq!(*count.lock().unwrap(), 3, "each item decoded exactly once");
    }

    #[test]
    fn stale_epoch_is_carried_on_the_outcome() {
        let decode: Arc<DecodeFn> = Arc::new(|_s, item, _, _, _, _| Ok(image(item, 16)));
        let (pool, rx) = DecodePool::new(2, 1 << 20, decode);
        let src = source();
        pool.set_targets(7, &src, &targets(&[0]));
        let o = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(o.key.epoch, 7);
    }

    #[test]
    fn byte_budget_does_not_stall_delivery() {
        // Budget smaller than the working set; slow draining must still complete.
        let decode: Arc<DecodeFn> = Arc::new(|_s, item, _, _, _, _| Ok(image(item, 256)));
        let (pool, rx) = DecodePool::new(3, 300, decode); // ~1 image of headroom
        let src = source();
        pool.set_targets(1, &src, &targets(&[0, 1, 2, 3, 4, 5]));
        let mut got = Vec::new();
        for _ in 0..6 {
            let o = rx.recv_timeout(Duration::from_secs(5)).expect("delivered");
            got.push(o.key.item);
            std::thread::sleep(Duration::from_millis(5)); // drain slowly
            drop(o); // frees budget
        }
        got.sort();
        assert_eq!(got, vec![0, 1, 2, 3, 4, 5]);
    }

    // ---- multi-consumer contract (task #83) ----

    #[test]
    fn same_item_display_and_thumb_coexist_and_both_decode() {
        let purposes = Arc::new(StdMutex::new(Vec::<Purpose>::new()));
        let rec = purposes.clone();
        let decode: Arc<DecodeFn> = Arc::new(move |_s, item, _, _, purpose, _| {
            rec.lock().unwrap().push(purpose);
            Ok(image(item, 16))
        });
        let (pool, rx) = DecodePool::new(2, 1 << 20, decode);
        let src = source();
        let mut wants = targets(&[7]);
        wants.push(Want::thumb(7, thumb_box()));
        pool.set_targets(1, &src, &wants);
        let got = drain_n(&rx, 2);
        assert_eq!(got, vec![7, 7], "both jobs for item 7 ran");
        let mut p = purposes.lock().unwrap().clone();
        p.sort_by_key(|p| *p == Purpose::Thumb);
        assert_eq!(p, vec![Purpose::Display, Purpose::Thumb]);
    }

    #[test]
    fn thumb_wants_never_cancel_display_jobs() {
        let display_decodes = Arc::new(StdMutex::new(0usize));
        let (started_tx, started_rx) = channel::<()>();
        let (release_tx, release_rx) = channel::<()>();
        let gate = Arc::new(AtomicBool::new(true));
        let release_rx = StdMutex::new(release_rx);
        let dd = display_decodes.clone();
        let decode: Arc<DecodeFn> = Arc::new(move |_s, item, _, _, purpose, _| {
            if purpose == Purpose::Display {
                *dd.lock().unwrap() += 1;
            }
            if gate.swap(false, Ordering::SeqCst) {
                started_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
            }
            Ok(image(item, 16))
        });
        let (pool, rx) = DecodePool::new(1, 1 << 20, decode);
        let src = source();
        pool.set_targets(1, &src, &targets(&[0, 1, 2]));
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap(); // 0 in-flight
                                                                  // Re-issue the same display wants PLUS thumb wants — the display jobs
                                                                  // (queued and in-flight) must be untouched: no cancellation, no re-decode.
        let mut wants = targets(&[0, 1, 2]);
        wants.push(Want::thumb(1, thumb_box()));
        wants.push(Want::thumb(2, thumb_box()));
        pool.set_targets(1, &src, &wants);
        release_tx.send(()).unwrap();
        drain_n(&rx, 5); // 3 display + 2 thumb
        assert_eq!(
            *display_decodes.lock().unwrap(),
            3,
            "each display item decoded exactly once — thumbs neither cancelled nor duped them"
        );
    }

    #[test]
    fn dropping_thumb_wants_leaves_display_untouched() {
        let decode: Arc<DecodeFn> = Arc::new(|_s, item, _, _, _, _| {
            std::thread::sleep(Duration::from_millis(10));
            Ok(image(item, 16))
        });
        let (pool, rx) = DecodePool::new(1, 1 << 20, decode);
        let src = source();
        let mut wants = targets(&[0, 1]);
        wants.push(Want::thumb(5, thumb_box()));
        pool.set_targets(1, &src, &wants);
        // Immediately drop the thumb want (e.g. the panel closed).
        pool.set_targets(1, &src, &targets(&[0, 1]));
        let mut got = drain_n(&rx, 2);
        got.sort();
        assert_eq!(got, vec![0, 1], "display outcomes still delivered");
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "the cancelled thumb never arrives"
        );
    }

    #[test]
    fn thumb_occupancy_cap_leaves_workers_for_display() {
        // 3 workers → thumb cap = 1. Gate every decode; queue 3 thumbs first,
        // then displays. Only ONE thumb may start; the displays must all start
        // even while the thumb is stuck.
        let started = Arc::new(StdMutex::new(Vec::<(usize, Purpose)>::new()));
        let (release_tx, release_rx) = channel::<()>();
        let release_rx = Arc::new(StdMutex::new(release_rx));
        let s = started.clone();
        let rr = release_rx.clone();
        let decode: Arc<DecodeFn> = Arc::new(move |_s, item, _, _, purpose, _| {
            s.lock().unwrap().push((item, purpose));
            rr.lock().unwrap().recv().unwrap();
            Ok(image(item, 16))
        });
        let (pool, rx) = DecodePool::new(3, 1 << 20, decode);
        let src = source();
        let wants: Vec<Want> = vec![
            Want::thumb(10, thumb_box()),
            Want::thumb(11, thumb_box()),
            Want::thumb(12, thumb_box()),
        ];
        pool.set_targets(1, &src, &wants);
        std::thread::sleep(Duration::from_millis(150));
        {
            let s = started.lock().unwrap();
            assert_eq!(s.len(), 1, "occupancy cap: one thumb in flight, got {s:?}");
            assert_eq!(s[0].1, Purpose::Thumb);
        }
        // Now displays arrive (still keeping the thumbs wanted, ahead of them
        // in real composition — order here puts displays first as AppCore does).
        let mut wants2 = targets(&[0, 1]);
        wants2.extend(wants);
        pool.set_targets(1, &src, &wants2);
        std::thread::sleep(Duration::from_millis(150));
        {
            let s = started.lock().unwrap();
            let displays = s.iter().filter(|(_, p)| *p == Purpose::Display).count();
            assert_eq!(
                displays, 2,
                "both display jobs started despite queued thumbs: {s:?}"
            );
        }
        // Release everyone (5 decodes total: 1 thumb + 2 displays running, then
        // 2 remaining thumbs as slots free).
        for _ in 0..5 {
            let _ = release_tx.send(());
        }
        drain_n(&rx, 5);
    }

    #[test]
    fn outcome_into_image_frees_budget() {
        let decode: Arc<DecodeFn> = Arc::new(|_s, item, _, _, _, _| Ok(image(item, 256)));
        // Budget of ~1 image: if into_image leaked the guard, the second decode
        // would park forever.
        let (pool, rx) = DecodePool::new(1, 300, decode);
        let src = source();
        pool.set_targets(1, &src, &targets(&[0, 1]));
        let o = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let img = o.into_image().expect("ok result");
        assert_eq!(img.pixels.len(), 256);
        let o2 = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("budget freed");
        drop(o2);
    }
}
