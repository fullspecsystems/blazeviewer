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
use pb_source::ItemSource;

/// Which consumer a decode serves. `Display` covers the viewer's whole ladder
/// (current / previews / sharp-ring fulls / video posters — their relative
/// priority is the want-list *order*); `Thumb` is the Thumbnails strip (task
/// #83). Separating consumers in the request identity is what guarantees thumb
/// demand can never displace display work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Purpose {
    Display,
    Thumb,
    /// A poster-selection walk (task #114): the ONE scored walk per movie that
    /// chooses the poster frame and cuts every consumer's artifact from it.
    /// Geometry-NEUTRAL — keyed by content generation, exempt from the epoch
    /// cancel in [`DecodePool::set_targets`] (a resize must not kill a
    /// multi-second walk whose choice is viewport-independent) — and
    /// class-mutable (thumb-cap occupancy until display demand joins; see
    /// `Want::poster_select`).
    PosterSelect,
}

/// The injected decode step (resolves the item's bytes from the source, then
/// decodes; a fake in tests). The pool is **source-agnostic** — it carries the
/// `ItemSource` and an item index, never a path, so a filesystem listing and a
/// ZIP archive flow through the same pool. The `bool` is `allow_preview`: true
/// requests a fast embedded preview where one exists (HEIC thumbnail, RAW
/// preview), false forces the full-resolution decode. The [`Purpose`] lets the
/// decode route format-specific fast paths (a thumb decode may use the EXIF
/// IFD1 thumbnail; a display decode never does). The `&AtomicBool` is the
/// job's cancel flag, set by `set_targets` when the item is no longer wanted —
/// long steps (the video poster walk, task #79) check it mid-job; single-shot
/// image decodes may ignore it (the result is discarded either way).
pub type DecodeFn = dyn Fn(
        &dyn ItemSource,
        usize,
        Option<FitBox>,
        bool,
        Purpose,
        &AtomicBool,
    ) -> Result<DecodedImage, DecodeError>
    + Send
    + Sync;

/// Identifies a unit of decode work: which item, for which consumer, at which
/// geometry epoch, in which representation. The epoch rides back on the [`Outcome`]
/// so the main thread can discard a result decoded for a stale geometry (after a
/// resize / fit toggle); the purpose routes the result to its consumer (ring vs
/// thumb cache); the `rep_kind` (#106.7 §5) keeps a full-res `Original` want and a
/// decode-to-`Fit` want for the *same* item from deduping each other away — the
/// parked full-res tier requests the Original alongside the on-screen Fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DecodeKey {
    pub item: usize,
    pub epoch: u64,
    pub purpose: Purpose,
    pub rep_kind: pb_core::RepKind,
}

/// The representation a decode produces: a decode-to-fit target (`fit = Some`) is a
/// `Fit` texture; a full-resolution decode (`fit = None`) is an `Original`.
fn rep_kind_of(fit: &Option<FitBox>) -> pb_core::RepKind {
    match fit {
        Some(_) => pb_core::RepKind::Fit,
        None => pb_core::RepKind::Original,
    }
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
    /// [`Purpose::PosterSelect`] only: the CONTENT generation this selection is
    /// for (its `DecodeKey.epoch` — selections are deck-scoped, not
    /// geometry-scoped). Ignored for other purposes.
    pub sel_gen: u64,
    /// [`Purpose::PosterSelect`] only: whether display demand exists. `false` =
    /// thumb-only, scheduled under the thumb occupancy cap; `true` = display
    /// class. Mutating this across `set_targets` calls PROMOTES a queued
    /// selection in place (identity unchanged, no restart — Codex #114 r3).
    pub display_class: bool,
    /// [`Purpose::PosterSelect`] only: the absolute replay locator
    /// `(origin_hns, relative_hns)` of an already-chosen frame (task #114
    /// phase 3). When set, the selection is a cheap decode-forward REPLAY of
    /// that exact frame, not a fresh scored walk — how an evicted artifact is
    /// re-obtained after resize/eviction.
    pub replay: Option<(i64, i64)>,
    /// [`Purpose::PosterSelect`] only: this job holds NATIVE-sized frame
    /// buffers while it runs (a native-variant walk, or any replay — both
    /// materialize a native frame). Capped at [`NATIVE_WALK_CAP`] concurrent by
    /// the scheduler (phase-2 RAM permit): the pool's `inflight_bytes` counts
    /// nothing until a decode RETURNS, so unbounded native walks would hold
    /// ~2 native buffers each invisibly.
    pub native_class: bool,
}

/// Max concurrent native-class selections (the phase-2 RAM permit: each holds
/// up to ~2 native RGBA buffers — best-so-far + current candidate — before any
/// byte is accounted).
pub const NATIVE_WALK_CAP: usize = 2;

impl Want {
    pub fn display(item: usize, fit: Option<FitBox>, preview: bool) -> Want {
        Want {
            item,
            fit,
            preview,
            purpose: Purpose::Display,
            sel_gen: 0,
            display_class: true,
            replay: None,
            native_class: false,
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
            sel_gen: 0,
            display_class: false,
            replay: None,
            native_class: false,
        }
    }

    /// A poster-selection walk (task #114). `fit` is the display fit the Fit
    /// artifact should be cut for (the pool tags that artifact with the geometry
    /// epoch at enqueue — it alone goes stale on resize; the choice does not).
    pub fn poster_select(
        item: usize,
        fit: Option<FitBox>,
        content_gen: u64,
        display_class: bool,
    ) -> Want {
        Want {
            item,
            fit,
            preview: false,
            purpose: Purpose::PosterSelect,
            sel_gen: content_gen,
            display_class,
            replay: None,
            native_class: false,
        }
    }

    /// Attach a replay locator (phase 3): the selection becomes a decode-forward
    /// replay of the already-chosen frame. Replays are native-class.
    pub fn with_replay(mut self, replay: Option<(i64, i64)>) -> Want {
        self.replay = replay;
        if replay.is_some() {
            self.native_class = true;
        }
        self
    }

    /// Mark the job native-class (the phase-2 native-walk variant).
    pub fn with_native_class(mut self, native: bool) -> Want {
        self.native_class = self.native_class || native;
        self
    }
}

/// The dedup identity of a want/job. Selections force `RepKind::Fit` so their
/// identity is stable regardless of the display mode's fit (`fit: None` in
/// Original mode would otherwise flip the rep and split the identity).
fn want_key(w: &Want) -> (usize, Purpose, pb_core::RepKind) {
    match w.purpose {
        Purpose::PosterSelect => (w.item, w.purpose, pb_core::RepKind::Fit),
        _ => (w.item, w.purpose, rep_kind_of(&w.fit)),
    }
}

/// A finished decode handed back to the main thread. Dropping it frees the item's
/// bytes from the pool's in-flight budget (RAII), so the upload→drop cycle is what
/// lets workers proceed.
pub struct Outcome {
    pub key: DecodeKey,
    /// The decoded image for `Display`/`Thumb` outcomes. INVARIANT: for a
    /// [`Purpose::PosterSelect`] outcome this is a placeholder `Err` no consumer
    /// may read — the drain matches `PosterSelect` FIRST and consumes
    /// [`selection`](Self::selection) instead (pinned by test).
    pub result: Result<DecodedImage, DecodeError>,
    /// The typed poster-selection payload (task #114). `Some` iff
    /// `key.purpose == PosterSelect`; its `key.epoch` is the CONTENT generation.
    pub selection: Option<Result<pb_decode::PosterSelection, DecodeError>>,
    /// The geometry epoch the job's `fit` was taken from — the Fit ARTIFACT's
    /// staleness tag for selections (Codex #114 r3: a selection survives a
    /// resize, its cut Fit does not). Equals `key.epoch` for non-selections.
    pub fit_tag_epoch: u64,
    /// The `FitBox` the job's artifacts were cut for — the OTHER half of the
    /// artifact tag (phase-1 review finding 1: a thumb-only walk promoted to
    /// display mid-flight has the right epoch but a ~thumb-sized Fit; the epoch
    /// alone would admit it as the display poster).
    pub fit_tag: Option<FitBox>,
    /// The JOB's `allow_preview` flag (not the image's `is_preview`). Load-bearing for the
    /// drain's upgrade bookkeeping: an `is_preview` image from a `preview: true` job landing on
    /// an already-resident preview is a DUPLICATE (the pool untracks a finished job before its
    /// outcome is drained, so a blaze-time re-issue can decode the same preview twice) and must
    /// be dropped — only an `is_preview` image from a `preview: false` job means "a genuine
    /// full request could only produce a preview" (the RAW case) and may end the sharpen loop.
    /// Conflating the two poisoned `upgrade_done` and left photos stuck blurry (2026-07-19).
    pub preview: bool,
    /// The pool's byte-budget RAII (freed on drop). `None` for a *synthetic* outcome not
    /// produced by the pool — a macOS archive-video poster the shell generated and fed back
    /// in (`Outcome::synthetic`); it carries no pool budget.
    _budget: Option<BudgetGuard>,
}

impl Outcome {
    /// A result that did **not** come from the decode pool — the macOS shell's archive-video
    /// poster (generated via `AVAssetImageGenerator`), fed into the resident ring through the
    /// normal `drain_results` upload path. Carries no pool byte-budget.
    pub fn synthetic(
        item: usize,
        epoch: u64,
        rep_kind: pb_core::RepKind,
        result: Result<DecodedImage, DecodeError>,
    ) -> Self {
        Outcome {
            key: DecodeKey {
                item,
                epoch,
                purpose: Purpose::Display,
                rep_kind,
            },
            result,
            selection: None,
            fit_tag_epoch: epoch,
            fit_tag: None,
            preview: false, // a synthetic poster is a definitive full, not a preview request
            _budget: None,
        }
    }

    /// [`synthetic`](Self::synthetic) that additionally **inherits `donor`'s
    /// pool byte-budget reservation** (phase-1 review finding 4): when a
    /// selection's Fit artifact re-enters the drain as a display outcome, the
    /// backpressure must follow the pixels — otherwise dropping the selection
    /// outcome releases the whole summed charge while the Fit still sits in
    /// `pending_uploads`.
    pub fn synthetic_from(
        donor: &mut Outcome,
        item: usize,
        epoch: u64,
        rep_kind: pb_core::RepKind,
        result: Result<DecodedImage, DecodeError>,
    ) -> Self {
        let mut o = Outcome::synthetic(item, epoch, rep_kind, result);
        o._budget = donor._budget.take();
        o
    }

    /// Carve `bytes` of `donor`'s budget reservation into a new synthetic
    /// outcome (phase 3: ONE selection payload fans into SEVERAL staged
    /// outcomes - the Fit and the Original each carry their own share of the
    /// backpressure; the remainder releases when the donor drops).
    pub fn synthetic_carved(
        donor: &mut Outcome,
        item: usize,
        epoch: u64,
        rep_kind: pb_core::RepKind,
        result: Result<DecodedImage, DecodeError>,
        bytes: usize,
    ) -> Self {
        let mut o = Outcome::synthetic(item, epoch, rep_kind, result);
        if let Some(g) = donor._budget.as_mut() {
            let carved = bytes.min(g.bytes);
            g.bytes -= carved;
            o._budget = Some(BudgetGuard {
                shared: g.shared.clone(),
                bytes: carved,
            });
        }
        o
    }

    /// Mark this (synthetic/test) outcome as produced by an `allow_preview` job — the
    /// preview-first request shape, for tests exercising the duplicate-preview drain rule.
    pub fn from_preview_request(mut self) -> Self {
        self.preview = true;
        self
    }

    /// A pool-less poster-selection outcome (task #114) for drain-routing tests:
    /// `gen` is the content generation (the selection's `key.epoch`),
    /// `fit_tag_epoch` the geometry epoch its Fit artifact was cut for.
    pub fn synthetic_selection(
        item: usize,
        gen: u64,
        fit_tag_epoch: u64,
        fit_tag: Option<FitBox>,
        selection: Result<pb_decode::PosterSelection, DecodeError>,
    ) -> Self {
        Outcome {
            key: DecodeKey {
                item,
                epoch: gen,
                purpose: Purpose::PosterSelect,
                rep_kind: pb_core::RepKind::Fit,
            },
            result: Err(DecodeError::Corrupt("poster-selection payload".into())),
            selection: Some(selection),
            fit_tag_epoch,
            fit_tag,
            preview: false,
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
    source: Arc<dyn ItemSource>,
    fit: Option<FitBox>,
    /// Whether to decode a fast preview (true) or the full resolution (false).
    preview: bool,
    prio: u32,
    cancel: Arc<AtomicBool>,
    /// The geometry epoch `fit` was taken from (selections: the Fit artifact's
    /// staleness tag; others: == key.epoch). Updated in place when a QUEUED
    /// selection is re-emitted after a resize.
    fit_tag_epoch: u64,
    /// Scheduling class for [`Purpose::PosterSelect`] jobs: `true` = thumb-only
    /// demand, counted under the thumb occupancy cap. Mutated in place by
    /// promotion (`set_targets` with `display_class: true`) while queued;
    /// meaningless for other purposes (their class IS their purpose).
    thumb_class: bool,
    /// The replay locator, when this selection is a decode-forward replay.
    replay: Option<(i64, i64)>,
    /// Native-RAM class (phase-2 permit): admission-gated by [`NATIVE_WALK_CAP`].
    native_class: bool,
}

/// A tracked (queued or in-flight) job's dedup entry: the cancel flag plus the
/// job's `key.epoch` (geometry epoch, or content generation for selections) so
/// `set_targets` can cancel a stale-generation selection whose replacement wants
/// the same identity (in-flight jobs aren't visible in `queue`).
struct TrackedEntry {
    flag: Arc<AtomicBool>,
    gen: u64,
}

struct Inner {
    queue: Vec<Job>,
    /// (item, purpose, rep_kind) -> entry, for every queued OR in-flight job
    /// (the dedup set). Purpose-keyed so consumers can't cancel each other (task #83);
    /// rep_kind-keyed so a Fit and an Original want for the same item coexist (#106.7).
    tracked: HashMap<(usize, Purpose, pb_core::RepKind), TrackedEntry>,
    /// Decoded-but-not-yet-drained bytes (the backpressure counter).
    inflight_bytes: usize,
    /// Thumb-purpose jobs currently decoding (the occupancy guard's counter).
    thumb_inflight: usize,
    /// Native-class selections currently decoding (the phase-2 RAM permit).
    native_inflight: usize,
    epoch: u64,
    shutdown: bool,
}

struct Shared {
    inner: Mutex<Inner>,
    cv: Condvar,
    decode: Arc<DecodeFn>,
    /// The poster-selection walk (task #114) — runs the ONE scored walk for a
    /// movie and returns the typed choice + artifacts. Injected like `decode`;
    /// the default (plain [`DecodePool::new`]) refuses, for callers/tests that
    /// never schedule selections.
    select: Arc<SelectFn>,
    results_tx: Sender<Outcome>,
    byte_budget: usize,
    /// Max concurrent thumb-purpose decodes: `max(1, workers - 2)`, so display
    /// jobs always find a free worker (the anti-inversion guard, task #83).
    thumb_cap: usize,
}

/// The injected poster-selection step (task #114): run the scored walk for
/// `item`, cutting artifacts for `fit` (the display fit, or the thumb fit for a
/// thumb-only selection). Long-running — MUST check the cancel flag between
/// samples, like the poster walk it wraps.
pub type SelectFn = dyn Fn(
        &dyn ItemSource,
        usize,
        Option<FitBox>,
        bool,               // display_class: whether the native winner is wanted
        Option<(i64, i64)>, // replay locator: decode-forward instead of walking
        &AtomicBool,
    ) -> Result<pb_decode::PosterSelection, DecodeError>
    + Send
    + Sync;

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
        Self::new_with_select(
            workers,
            byte_budget,
            decode,
            Arc::new(|_: &dyn ItemSource, _, _, _, _, _: &AtomicBool| {
                Err(DecodeError::Corrupt("no selection fn installed".into()))
            }),
        )
    }

    /// [`new`](Self::new) plus the poster-selection walk (task #114). The app
    /// composes both; plain `new` keeps every selection-free caller/test intact.
    pub fn new_with_select(
        workers: usize,
        byte_budget: usize,
        decode: Arc<DecodeFn>,
        select: Arc<SelectFn>,
    ) -> (Self, Receiver<Outcome>) {
        let workers = workers.max(1);
        let (results_tx, results_rx) = channel();
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                queue: Vec::new(),
                tracked: HashMap::new(),
                inflight_bytes: 0,
                thumb_inflight: 0,
                native_inflight: 0,
                epoch: 0,
                shutdown: false,
            }),
            cv: Condvar::new(),
            decode,
            select,
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
    pub fn set_targets(&self, epoch: u64, source: &Arc<dyn ItemSource>, prioritized: &[Want]) {
        let mut inner = self.shared.inner.lock().unwrap();

        if epoch != inner.epoch {
            // Geometry changed: every queued/in-flight job is for the old size —
            // EXCEPT poster selections (task #114): a selection's choice is
            // viewport-independent and keyed by content generation, so a resize
            // must not kill a multi-second walk. Its Fit ARTIFACT carries its own
            // staleness tag (`fit_tag_epoch`) instead.
            for (key, entry) in inner.tracked.iter() {
                if key.1 != Purpose::PosterSelect {
                    entry.flag.store(true, Ordering::Release);
                }
            }
            inner
                .queue
                .retain(|j| j.key.purpose == Purpose::PosterSelect);
            inner
                .tracked
                .retain(|key, _| key.1 == Purpose::PosterSelect);
            inner.epoch = epoch;
        }

        // Want lookup: priority + the want itself, LAST duplicate wins (the
        // preview/full pair for one item shares an identity; parity with the old
        // collect() behavior). A selection want also carries its content
        // generation: a tracked selection from the previous deck must be
        // cancelled and replaced, never deduped against.
        let mut wanted: HashMap<(usize, Purpose, pb_core::RepKind), (u32, &Want)> =
            HashMap::with_capacity(prioritized.len());
        for (i, w) in prioritized.iter().enumerate() {
            wanted.insert(want_key(w), (i as u32, w));
        }
        let gen_ok = |key: &(usize, Purpose, pb_core::RepKind), gen: u64| match wanted.get(key) {
            Some((_, w)) if key.1 == Purpose::PosterSelect => w.sel_gen == gen,
            Some(_) => true,
            None => false,
        };

        // Cancel anything no longer wanted (or wanted at a different selection
        // generation); drop those still queued.
        for (key, entry) in inner.tracked.iter() {
            if !gen_ok(key, entry.gen) {
                entry.flag.store(true, Ordering::Release);
            }
        }
        inner
            .queue
            .retain(|j| gen_ok(&(j.key.item, j.key.purpose, j.key.rep_kind), j.key.epoch));
        let live: std::collections::HashSet<(usize, Purpose, pb_core::RepKind)> = inner
            .queue
            .iter()
            .map(|j| (j.key.item, j.key.purpose, j.key.rep_kind))
            .collect();
        inner.tracked.retain(|key, entry| {
            gen_ok(key, entry.gen) && (live.contains(key) || !entry.flag.load(Ordering::Acquire))
        });

        // Re-prioritize jobs still queued. A queued selection also refreshes its
        // Fit-artifact target + geometry tag, and its scheduling class in place —
        // PROMOTION (Codex #114 r3): flipping thumb_class -> display is what
        // unparks it from the thumb cap (the trailing notify_all wakes workers);
        // an already-RUNNING selection is deliberately untouched (it keeps the
        // slot it was admitted with — `took_thumb_slot`).
        for job in inner.queue.iter_mut() {
            if let Some(&(prio, w)) = wanted.get(&(job.key.item, job.key.purpose, job.key.rep_kind))
            {
                job.prio = prio;
                if job.key.purpose == Purpose::PosterSelect {
                    job.fit = w.fit;
                    job.fit_tag_epoch = epoch;
                    job.thumb_class = !w.display_class;
                    // A hint is only ever GAINED by re-emission: a later pass
                    // that lost sight of the choice (the ledger reopened) must
                    // not strip a queued replay back into a full walk.
                    if w.replay.is_some() {
                        job.replay = w.replay;
                    }
                    job.native_class = w.native_class || job.replay.is_some();
                }
            }
        }

        // Enqueue newly-wanted items (dedup against queued + in-flight; a
        // selection dedups only against its own generation).
        for w in prioritized {
            let key = want_key(w);
            let job_gen = if w.purpose == Purpose::PosterSelect {
                w.sel_gen
            } else {
                epoch
            };
            if inner
                .tracked
                .get(&key)
                .is_some_and(|e| key.1 != Purpose::PosterSelect || e.gen == job_gen)
            {
                continue;
            }
            let flag = Arc::new(AtomicBool::new(false));
            inner.tracked.insert(
                key,
                TrackedEntry {
                    flag: flag.clone(),
                    gen: job_gen,
                },
            );
            let prio = wanted[&key].0;
            inner.queue.push(Job {
                key: DecodeKey {
                    item: w.item,
                    epoch: job_gen,
                    purpose: w.purpose,
                    rep_kind: key.2,
                },
                source: source.clone(),
                fit: w.fit,
                preview: w.preview,
                prio,
                cancel: flag,
                fit_tag_epoch: epoch,
                thumb_class: w.purpose == Purpose::PosterSelect && !w.display_class,
                replay: w.replay,
                native_class: w.purpose == Purpose::PosterSelect && w.native_class,
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
                        if takes_thumb_slot(&job) {
                            inner.thumb_inflight += 1;
                        }
                        if job.native_class {
                            inner.native_inflight += 1;
                        }
                        break job;
                    }
                }
                inner = shared.cv.wait(inner).unwrap();
            }
        };
        // Class-at-admission (Codex #114 r3): the slot this job took is what it
        // releases, even if a promotion lands while it runs.
        let took_thumb_slot = takes_thumb_slot(&job);
        let took_native_slot = job.native_class;

        // Cancelled before it ran: forget it and move on.
        if job.cancel.load(Ordering::Acquire) {
            let mut inner = shared.inner.lock().unwrap();
            untrack(&mut inner, &job);
            release_thumb_slot(&mut inner, took_thumb_slot);
            release_native_slot(&mut inner, took_native_slot);
            drop(inner);
            shared.cv.notify_all();
            continue;
        }

        // Dispatch by work kind: a poster selection runs the injected walk and
        // produces the typed payload; everything else is a plain image decode.
        let (result, selection, bytes) = if job.key.purpose == Purpose::PosterSelect {
            let sel = (shared.select)(
                job.source.as_ref(),
                job.key.item,
                job.fit,
                !job.thumb_class,
                job.replay,
                &job.cancel,
            );
            let bytes = sel.as_ref().map(|s| s.pixel_bytes()).unwrap_or(0);
            // The placeholder no consumer may read (the drain matches
            // PosterSelect before touching `result` — pinned by test).
            let placeholder = Err(DecodeError::Corrupt("poster-selection payload".into()));
            (placeholder, Some(sel), bytes)
        } else {
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
            (result, None, bytes)
        };

        // Account for the result and stop tracking the item — unless it was
        // cancelled mid-decode, in which case discard the result entirely.
        // A SELECTION keeps its dedup entry until the outcome has been SENT
        // (phase-1 review finding 5): untracking before the send opens a window
        // where a prefetch pass sees no tracked job while the payload sits
        // undrained in the channel — and starts a second full walk.
        let is_selection = job.key.purpose == Purpose::PosterSelect;
        {
            let mut inner = shared.inner.lock().unwrap();
            if !is_selection {
                untrack(&mut inner, &job);
            }
            release_thumb_slot(&mut inner, took_thumb_slot);
            release_native_slot(&mut inner, took_native_slot);
            if job.cancel.load(Ordering::Acquire) {
                if is_selection {
                    untrack(&mut inner, &job);
                }
                drop(inner);
                shared.cv.notify_all();
                continue;
            }
            inner.inflight_bytes += bytes;
        }
        // A freed thumb/native slot may unblock a parked worker even while the
        // byte budget is unchanged.
        if took_thumb_slot || took_native_slot {
            shared.cv.notify_all();
        }

        let outcome = Outcome {
            key: job.key,
            result,
            selection,
            fit_tag_epoch: job.fit_tag_epoch,
            fit_tag: job.fit,
            preview: job.preview,
            _budget: Some(BudgetGuard {
                shared: shared.clone(),
                bytes,
            }),
        };
        if shared.results_tx.send(outcome).is_err() {
            return; // receiver gone; the guard frees the bytes as it drops
        }
        if is_selection {
            let mut inner = shared.inner.lock().unwrap();
            untrack(&mut inner, &job);
            drop(inner);
            shared.cv.notify_all();
        }
    }
}

fn release_thumb_slot(inner: &mut Inner, took_thumb_slot: bool) {
    if took_thumb_slot {
        inner.thumb_inflight = inner.thumb_inflight.saturating_sub(1);
    }
}

fn release_native_slot(inner: &mut Inner, took_native_slot: bool) {
    if took_native_slot {
        inner.native_inflight = inner.native_inflight.saturating_sub(1);
    }
}

/// Whether a job occupies a thumb slot at admission: every Thumb-purpose job,
/// plus a poster selection with **thumb-only demand** (task #114 — far-away
/// movies must not occupy every worker; display-class selections schedule like
/// display work).
fn takes_thumb_slot(j: &Job) -> bool {
    j.key.purpose == Purpose::Thumb || (j.key.purpose == Purpose::PosterSelect && j.thumb_class)
}

/// Remove the dedup entry for the job's `(item, purpose)` only if it still maps
/// to *this* job's flag. A newer job for the same key (e.g. re-requested after
/// an epoch change while this one was cancelled mid-decode) must keep its own
/// entry so it can still be deduped and cancelled.
fn untrack(inner: &mut Inner, job: &Job) {
    let key = (job.key.item, job.key.purpose, job.key.rep_kind);
    if inner
        .tracked
        .get(&key)
        .is_some_and(|e| Arc::ptr_eq(&e.flag, &job.cancel))
    {
        inner.tracked.remove(&key);
    }
}

/// Remove and return the highest-priority (lowest `prio`) *runnable* job:
/// thumb-slot jobs (thumb fills + thumb-only selections) are skipped while the
/// occupancy cap is reached, so a display job behind a queue of thumbs still
/// runs immediately.
fn pop_best(inner: &mut Inner, thumb_cap: usize) -> Option<Job> {
    let thumbs_blocked = inner.thumb_inflight >= thumb_cap;
    let native_blocked = inner.native_inflight >= NATIVE_WALK_CAP;
    let idx = inner
        .queue
        .iter()
        .enumerate()
        .filter(|(_, j)| !(thumbs_blocked && takes_thumb_slot(j)))
        // The phase-2 RAM permit: native-class selections hold native-sized
        // buffers the byte budget can't see until they return — cap them at
        // the ADMISSION level so lighter work keeps flowing past them.
        .filter(|(_, j)| !(native_blocked && j.native_class))
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
    impl ItemSource for FakeSource {
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

    fn source() -> Arc<dyn ItemSource> {
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
            native_inflight: 0,
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
                rep_kind: pb_core::RepKind::Original,
            },
            source: source(),
            fit: None,
            preview: false,
            prio: 0,
            cancel: flag.clone(),
            fit_tag_epoch: 0,
            thumb_class: false,
            replay: None,
            native_class: false,
        };
        let k = (5, Purpose::Display, pb_core::RepKind::Original);
        // A fresh job (re-requested after an epoch change) now owns item 5.
        inner.tracked.insert(
            k,
            TrackedEntry {
                flag: new.clone(),
                gen: 0,
            },
        );
        // The old, cancelled job finishing must NOT drop the new job's entry.
        untrack(&mut inner, &job(&old));
        assert!(inner.tracked.contains_key(&k));
        // The owning job removes its own entry.
        untrack(&mut inner, &job(&new));
        assert!(!inner.tracked.contains_key(&k));
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

    fn selection_output(item: usize) -> pb_decode::PosterSelection {
        pb_decode::PosterSelection {
            choice: pb_decode::PosterChoice {
                origin_hns: 0,
                relative_hns: item as i64 * 10_000_000,
                native_w: 1920,
                native_h: 1080,
                content_hdr: false,
            },
            fit_img: Some(image(item, 32)),
            thumb_img: Some(image(item, 8)),
            native: None,
        }
    }

    #[test]
    fn a_selection_outcome_carries_the_typed_payload_and_tags() {
        let decode: Arc<DecodeFn> = Arc::new(|_s, item, _, _, _, _| Ok(image(item, 16)));
        let select: Arc<SelectFn> = Arc::new(|_s, item, _, _, _, _| Ok(selection_output(item)));
        let (pool, rx) = DecodePool::new_with_select(2, 1 << 20, decode, select);
        let src = source();
        pool.set_targets(7, &src, &[Want::poster_select(3, None, 42, true)]);
        let o = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(o.key.purpose, Purpose::PosterSelect);
        assert_eq!(
            o.key.epoch, 42,
            "a selection's key carries the CONTENT generation"
        );
        assert_eq!(
            o.fit_tag_epoch, 7,
            "the Fit artifact is tagged with the geometry epoch"
        );
        let sel = o
            .selection
            .as_ref()
            .expect("typed payload present")
            .as_ref()
            .expect("selection ok");
        assert_eq!(sel.choice.native_w, 1920);
        assert_eq!(sel.pixel_bytes(), 32 + 8, "summed artifact bytes");
        assert!(
            o.result.is_err(),
            "the image slot is a placeholder no consumer may read"
        );
    }

    #[test]
    fn a_selection_survives_a_geometry_epoch_change() {
        // The walk blocks in-flight; a geometry epoch change (resize) cancels
        // ordinary jobs but the selection completes and delivers its payload.
        let (started_tx, started_rx) = channel::<()>();
        let (release_tx, release_rx) = channel::<()>();
        let release_rx = StdMutex::new(release_rx);
        let decode: Arc<DecodeFn> = Arc::new(|_s, item, _, _, _, _| Ok(image(item, 16)));
        let select: Arc<SelectFn> = Arc::new(move |_s, item, _, _, _, _| {
            started_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
            Ok(selection_output(item))
        });
        let (pool, rx) = DecodePool::new_with_select(1, 1 << 20, decode, select);
        let src = source();
        pool.set_targets(
            1,
            &src,
            &[
                Want::poster_select(5, None, 1, true),
                Want::display(9, None, false),
            ],
        );
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap(); // walk in flight
                                                                  // Resize: new epoch, selection re-emitted (level-triggered), item 9 gone.
        pool.set_targets(2, &src, &[Want::poster_select(5, None, 1, true)]);
        release_tx.send(()).unwrap();
        let o = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(o.key.purpose, Purpose::PosterSelect, "the walk survived");
        assert!(o.selection.is_some());
        // No duplicate selection, and item 9's job died with its epoch.
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
    }

    #[test]
    fn a_selection_not_re_emitted_is_cancelled() {
        // Level-triggered contract: a set_targets WITHOUT the selection cancels
        // it, and the finished walk's payload is discarded.
        let (started_tx, started_rx) = channel::<()>();
        let (release_tx, release_rx) = channel::<()>();
        let release_rx = StdMutex::new(release_rx);
        let decode: Arc<DecodeFn> = Arc::new(|_s, item, _, _, _, _| Ok(image(item, 16)));
        let select: Arc<SelectFn> = Arc::new(move |_s, item, _, _, _, _| {
            started_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
            Ok(selection_output(item))
        });
        let (pool, rx) = DecodePool::new_with_select(1, 1 << 20, decode, select);
        let src = source();
        pool.set_targets(1, &src, &[Want::poster_select(5, None, 1, true)]);
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        pool.set_targets(1, &src, &targets(&[9])); // selection absent -> cancelled
        release_tx.send(()).unwrap();
        let o = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(o.key.item, 9, "only the live display job delivers");
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
    }

    #[test]
    fn a_new_content_generation_replaces_a_tracked_selection() {
        // A deck swap re-emits the selection at a new content generation: the
        // old walk is cancelled (payload discarded), the new one runs fresh.
        let (started_tx, started_rx) = channel::<()>();
        let (release_tx, release_rx) = channel::<()>();
        let gate = Arc::new(AtomicBool::new(true)); // only the first walk blocks
        let release_rx = StdMutex::new(release_rx);
        let count = Arc::new(StdMutex::new(0usize));
        let c = count.clone();
        let decode: Arc<DecodeFn> = Arc::new(|_s, item, _, _, _, _| Ok(image(item, 16)));
        let select: Arc<SelectFn> = Arc::new(move |_s, item, _, _, _, _| {
            *c.lock().unwrap() += 1;
            if gate.swap(false, Ordering::SeqCst) {
                started_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
            }
            Ok(selection_output(item))
        });
        let (pool, rx) = DecodePool::new_with_select(1, 1 << 20, decode, select);
        let src = source();
        pool.set_targets(1, &src, &[Want::poster_select(5, None, 1, true)]);
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap(); // gen-1 walk in flight
        pool.set_targets(1, &src, &[Want::poster_select(5, None, 2, true)]); // new deck
        release_tx.send(()).unwrap();
        let o = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(o.key.epoch, 2, "only the new generation's payload delivers");
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
        assert_eq!(
            *count.lock().unwrap(),
            2,
            "both walks ran; the stale one was discarded"
        );
    }

    #[test]
    fn a_thumb_only_selection_waits_behind_the_thumb_cap_until_promoted() {
        // workers=3 => thumb_cap=1. A blocking thumb decode fills the cap; a
        // thumb-only selection stays parked; PROMOTION (display demand joins,
        // same identity) unparks it while the thumb still blocks.
        let (started_tx, started_rx) = channel::<()>();
        let (release_tx, release_rx) = channel::<()>();
        let release_rx = StdMutex::new(release_rx);
        let decode: Arc<DecodeFn> = Arc::new(move |_s, item, _, _, purpose, _| {
            if purpose == Purpose::Thumb && item == 0 {
                started_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
            }
            Ok(image(item, 16))
        });
        let select: Arc<SelectFn> = Arc::new(|_s, item, _, _, _, _| Ok(selection_output(item)));
        let (pool, rx) = DecodePool::new_with_select(3, 1 << 20, decode, select);
        let src = source();
        let thumb = Want::thumb(0, thumb_box());
        pool.set_targets(1, &src, &[thumb, Want::poster_select(5, None, 1, false)]);
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap(); // cap (1) is full
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "a thumb-only selection is parked behind the thumb cap"
        );
        pool.set_targets(1, &src, &[thumb, Want::poster_select(5, None, 1, true)]);
        let o = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            o.key.purpose,
            Purpose::PosterSelect,
            "promotion unparked the selection without a restart"
        );
        release_tx.send(()).unwrap();
        let o = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(o.key.item, 0, "the blocked thumb still lands");
    }

    #[test]
    fn native_class_selections_park_at_the_permit_while_light_work_flows() {
        // NATIVE_WALK_CAP = 2 (the phase-2 RAM permit): two blocking native
        // walks fill it; a third native selection stays queued while ordinary
        // display work flows past; a freed permit admits the third.
        let (started_tx, started_rx) = channel::<()>();
        let (release_tx, release_rx) = channel::<()>();
        let release_rx = StdMutex::new(release_rx);
        let decode: Arc<DecodeFn> = Arc::new(|_s, item, _, _, _, _| Ok(image(item, 16)));
        let select: Arc<SelectFn> = Arc::new(move |_s, item, _, _, _, _| {
            started_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
            Ok(selection_output(item))
        });
        let (pool, rx) = DecodePool::new_with_select(4, 1 << 20, decode, select);
        let src = source();
        let native = |i| Want::poster_select(i, None, 1, true).with_native_class(true);
        pool.set_targets(
            1,
            &src,
            &[
                native(1),
                native(2),
                native(3),
                Want::display(9, None, false),
            ],
        );
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap(); // permit full
        let o = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            o.key.item, 9,
            "light work flows past the parked native walk"
        );
        release_tx.send(()).unwrap(); // one walk finishes -> a permit frees
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap(); // third admits
        release_tx.send(()).unwrap();
        release_tx.send(()).unwrap();
        let mut got = drain_n(&rx, 3);
        got.sort();
        assert_eq!(got, vec![1, 2, 3], "all three selections deliver");
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
