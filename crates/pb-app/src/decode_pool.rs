//! Priority decode worker pool (plan §3.2 / §3.4).
//!
//! Decode + file I/O run here, **never on the event loop**. The pool pulls the
//! highest-priority job (priority = prefetch want-list index, 0 = the on-screen
//! image), reads the bytes off disk, decodes-to-fit, and ships the result back
//! over a channel for the main thread to upload during prefetch.
//!
//! Three properties make it safe under fast navigation:
//! - **Priority + dedup:** the current image jumps the queue; an item already
//!   queued or in-flight is never decoded twice.
//! - **Cancellation:** `set_targets` flags jobs no longer wanted; queued ones are
//!   dropped and an in-flight one's result is discarded when it finishes.
//! - **Byte-budget backpressure:** workers park rather than decode further ahead
//!   than the uploader can drain, so memory stays bounded no matter how deep the
//!   prefetch window is (worker count is capped too — see `recommended_workers`).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use pb_decode::{DecodeError, DecodedImage, FitBox};

/// The injected decode step (`decode_image_file` in the app; a fake in tests).
pub type DecodeFn =
    dyn Fn(&Path, Option<FitBox>) -> Result<DecodedImage, DecodeError> + Send + Sync;

/// Identifies a unit of decode work: which item, at which geometry epoch. The
/// epoch rides back on the [`Outcome`] so the main thread can discard a result
/// decoded for a stale geometry (after a resize / fit toggle).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DecodeKey {
    pub item: usize,
    pub epoch: u64,
}

/// A finished decode handed back to the main thread. Dropping it frees the item's
/// bytes from the pool's in-flight budget (RAII), so the upload→drop cycle is what
/// lets workers proceed.
pub struct Outcome {
    pub key: DecodeKey,
    pub result: Result<DecodedImage, DecodeError>,
    _budget: BudgetGuard,
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
    path: Arc<Path>,
    fit: Option<FitBox>,
    prio: u32,
    cancel: Arc<AtomicBool>,
}

struct Inner {
    queue: Vec<Job>,
    /// item -> cancel flag, for every queued OR in-flight job (the dedup set).
    tracked: HashMap<usize, Arc<AtomicBool>>,
    /// Decoded-but-not-yet-drained bytes (the backpressure counter).
    inflight_bytes: usize,
    epoch: u64,
    shutdown: bool,
}

struct Shared {
    inner: Mutex<Inner>,
    cv: Condvar,
    decode: Arc<DecodeFn>,
    results_tx: Sender<Outcome>,
    byte_budget: usize,
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
        let (results_tx, results_rx) = channel();
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                queue: Vec::new(),
                tracked: HashMap::new(),
                inflight_bytes: 0,
                epoch: 0,
                shutdown: false,
            }),
            cv: Condvar::new(),
            decode,
            results_tx,
            byte_budget: byte_budget.max(1),
        });
        let workers = (0..workers.max(1))
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
    pub fn set_targets(&self, epoch: u64, prioritized: &[(usize, Arc<Path>, Option<FitBox>)]) {
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

        let wanted: HashMap<usize, u32> = prioritized
            .iter()
            .enumerate()
            .map(|(i, (item, _, _))| (*item, i as u32))
            .collect();

        // Cancel anything no longer wanted; drop those still queued.
        for (item, flag) in inner.tracked.iter() {
            if !wanted.contains_key(item) {
                flag.store(true, Ordering::Release);
            }
        }
        inner.queue.retain(|j| wanted.contains_key(&j.key.item));
        let live: std::collections::HashSet<usize> =
            inner.queue.iter().map(|j| j.key.item).collect();
        inner.tracked.retain(|item, flag| {
            wanted.contains_key(item) && (live.contains(item) || !flag.load(Ordering::Acquire))
        });

        // Re-prioritize jobs still queued.
        for job in inner.queue.iter_mut() {
            if let Some(&prio) = wanted.get(&job.key.item) {
                job.prio = prio;
            }
        }

        // Enqueue newly-wanted items (dedup against queued + in-flight).
        for (item, path, fit) in prioritized {
            if inner.tracked.contains_key(item) {
                continue;
            }
            let flag = Arc::new(AtomicBool::new(false));
            inner.tracked.insert(*item, flag.clone());
            let prio = wanted[item];
            inner.queue.push(Job {
                key: DecodeKey { item: *item, epoch },
                path: path.clone(),
                fit: *fit,
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
        // Wait for a runnable job: one exists AND we're under the byte budget.
        let job = {
            let mut inner = shared.inner.lock().unwrap();
            loop {
                if inner.shutdown {
                    return;
                }
                if inner.inflight_bytes < shared.byte_budget {
                    if let Some(job) = pop_best(&mut inner.queue) {
                        break job;
                    }
                }
                inner = shared.cv.wait(inner).unwrap();
            }
        };

        // Cancelled before it ran: forget it and move on.
        if job.cancel.load(Ordering::Acquire) {
            let mut inner = shared.inner.lock().unwrap();
            inner.tracked.remove(&job.key.item);
            continue;
        }

        let result = (shared.decode)(&job.path, job.fit);
        let bytes = match &result {
            Ok(img) => img.pixels.len(),
            Err(_) => 0,
        };

        // Account for the result and stop tracking the item — unless it was
        // cancelled mid-decode, in which case discard the result entirely.
        {
            let mut inner = shared.inner.lock().unwrap();
            inner.tracked.remove(&job.key.item);
            if job.cancel.load(Ordering::Acquire) {
                continue;
            }
            inner.inflight_bytes += bytes;
        }

        let outcome = Outcome {
            key: job.key,
            result,
            _budget: BudgetGuard {
                shared: shared.clone(),
                bytes,
            },
        };
        if shared.results_tx.send(outcome).is_err() {
            return; // receiver gone; the guard frees the bytes as it drops
        }
    }
}

/// Remove and return the highest-priority (lowest `prio`) job.
fn pop_best(queue: &mut Vec<Job>) -> Option<Job> {
    let idx = queue
        .iter()
        .enumerate()
        .min_by_key(|(_, j)| j.prio)
        .map(|(i, _)| i)?;
    Some(queue.swap_remove(idx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pb_decode::PixelFormat;
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    fn path_for(item: usize) -> Arc<Path> {
        Arc::from(PathBuf::from(item.to_string()))
    }

    fn item_of(path: &Path) -> usize {
        path.file_name().unwrap().to_str().unwrap().parse().unwrap()
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
        }
    }

    fn targets(items: &[usize]) -> Vec<(usize, Arc<Path>, Option<FitBox>)> {
        items.iter().map(|&i| (i, path_for(i), None)).collect()
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
    fn delivers_all_wanted_items() {
        let decode: Arc<DecodeFn> = Arc::new(|p, _| Ok(image(item_of(p), 16)));
        let (pool, rx) = DecodePool::new(3, 1 << 20, decode);
        pool.set_targets(1, &targets(&[0, 1, 2, 3, 4]));
        let mut got = drain_n(&rx, 5);
        got.sort();
        assert_eq!(got, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn decodes_in_priority_order_with_one_worker() {
        let order = Arc::new(StdMutex::new(Vec::<usize>::new()));
        let rec = order.clone();
        let decode: Arc<DecodeFn> = Arc::new(move |p, _| {
            rec.lock().unwrap().push(item_of(p));
            Ok(image(item_of(p), 16))
        });
        let (pool, rx) = DecodePool::new(1, 1 << 20, decode);
        pool.set_targets(1, &targets(&[0, 1, 2, 3]));
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
        let decode: Arc<DecodeFn> = Arc::new(move |p, _| {
            if gate.swap(false, Ordering::SeqCst) {
                started_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
            }
            Ok(image(item_of(p), 16))
        });
        let (pool, rx) = DecodePool::new(1, 1 << 20, decode);
        pool.set_targets(1, &targets(&[0, 1, 2, 3, 4]));
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap(); // item 0 in-flight
                                                                  // Swap to a disjoint set: 0 (in-flight) + 1..4 (queued) are all cancelled.
        pool.set_targets(1, &targets(&[10, 11]));
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
        let decode: Arc<DecodeFn> = Arc::new(move |p, _| {
            *c.lock().unwrap() += 1;
            if gate.swap(false, Ordering::SeqCst) {
                started_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
            }
            Ok(image(item_of(p), 16))
        });
        let (pool, rx) = DecodePool::new(1, 1 << 20, decode);
        pool.set_targets(1, &targets(&[0, 1, 2]));
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // Re-request the same set while 0 is in-flight and 1,2 are queued.
        pool.set_targets(1, &targets(&[0, 1, 2]));
        release_tx.send(()).unwrap();
        drain_n(&rx, 3);
        assert_eq!(*count.lock().unwrap(), 3, "each item decoded exactly once");
    }

    #[test]
    fn stale_epoch_is_carried_on_the_outcome() {
        let decode: Arc<DecodeFn> = Arc::new(|p, _| Ok(image(item_of(p), 16)));
        let (pool, rx) = DecodePool::new(2, 1 << 20, decode);
        pool.set_targets(7, &targets(&[0]));
        let o = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(o.key.epoch, 7);
    }

    #[test]
    fn byte_budget_does_not_stall_delivery() {
        // Budget smaller than the working set; slow draining must still complete.
        let decode: Arc<DecodeFn> = Arc::new(|p, _| Ok(image(item_of(p), 256)));
        let (pool, rx) = DecodePool::new(3, 300, decode); // ~1 image of headroom
        pool.set_targets(1, &targets(&[0, 1, 2, 3, 4, 5]));
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
}
