//! Thumbnails-strip orchestration (task #83): the RAM-only thumb store
//! ([`pb_core::ThumbCache`] policy + RGBA8 payloads), the dedicated **derive
//! thread** the T0 post-upload handoff feeds, the shell-reported demand window,
//! and the [`FollowState`] auto-follow. All RAM, dropped on exit (ADR-018).
//!
//! Threading: `offer` runs on the event loop but is O(1) — a bounded
//! `try_send` of an already-owned buffer (the ring upload just finished with
//! it; it would otherwise drop). The resize + color bake happen on the derive
//! thread; results come back over a channel `poll`ed by the tick. A full
//! channel drops the offer — thumbs are best-effort; the warm-window fill
//! regenerates misses once parked.

use std::collections::HashSet;
use std::sync::mpsc::{sync_channel, Receiver, Sender, SyncSender};

use pb_core::{InsertOutcome, ThumbCache, ThumbDemand, ThumbTier};
use pb_decode::{DecodedImage, FitBox, PixelFormat};

use crate::follow::{FollowState, ScrollTo};

/// Generated-thumb ceiling (long edge). 512 — not 384 — because the panel is
/// resizable and stretching it for a bigger look is a wanted feature; embedded
/// previews are stored at their native size regardless (plan Rev 2).
pub const THUMB_EDGE: u32 = 512;
/// Canonical CPU cache budget: ~256 MB ≈ 256 worst-case (square) thumbs, ~365
/// typical 3:2. Config-constant, A/B-able like everything else.
pub const THUMB_BUDGET_BYTES: u64 = 256_000_000;
/// Warm window each side of the current item.
pub const THUMB_WARM: usize = 64;
/// Fill-plan cap per prefetch pump — bounds pool-queue churn, not coverage
/// (the next pump plans the next slice).
pub const THUMB_MAX_FILL_JOBS: usize = 24;
/// Derive-channel depth: the in-flight byte bound (2–3 full-res buffers).
const DERIVE_QUEUE: usize = 3;

/// The thumb fit box handed to Thumb-purpose pool decodes.
pub fn thumb_fit() -> FitBox {
    FitBox {
        max_width: THUMB_EDGE,
        max_height: THUMB_EDGE,
    }
}

/// A stored thumb's pixels + the source facts the jump-preview present needs
/// (real original dimensions + codec, so the info panel and the renderer see
/// the truth, not the thumb's own tiny size).
pub struct ThumbPixels {
    pub rgba: Vec<u8>,
    pub orig_w: u32,
    pub orig_h: u32,
    pub codec: &'static str,
}

struct DeriveJob {
    deck_gen: u64,
    item: usize,
    img: DecodedImage,
}

struct DeriveResult {
    deck_gen: u64,
    item: usize,
    thumb: DecodedImage,
}

/// The strip's orchestration state, owned by `AppCore`.
pub struct Thumbs {
    pub cache: ThumbCache<ThumbPixels>,
    /// Capture is enabled by the first panel open this session; before that the
    /// strip costs zero RAM and zero pool/derive time (plan: cost gating).
    pub enabled: bool,
    /// Items whose *thumb* fill decode failed — never re-planned (no retry loop).
    /// Distinct from `AppCore::failed` (display failures), which is also skipped.
    pub failed: HashSet<usize>,
    /// Auto-follow machine; `pending_scroll` is the one-slot command the shell
    /// pulls (a newer command supersedes an unpulled older one by design).
    pub follow: FollowState,
    pub pending_scroll: Option<ScrollTo>,
    /// Shell-reported `(visible, overscan)` inclusive index ranges; `None` until
    /// the shell first reports (then demand centers on the current item).
    pub viewport: Option<((usize, usize), (usize, usize))>,
    tx: Option<SyncSender<DeriveJob>>,
    rx: Option<Receiver<DeriveResult>>,
    /// Offers sent minus results received — keeps the host pump polling while a
    /// derive is in flight.
    in_flight: usize,
    /// Items with an accepted offer whose derived tile has not yet landed in
    /// the cache (task #114, review f8): without this, a `Chosen` selection
    /// whose tile is still in the derive queue reads as EVICTED to the fill
    /// planner and triggers another full walk. Keyed by `(deck generation, item)`
    /// (#119, Codex r2 h2): a LATE stale-generation result must retire only its
    /// own generation's marker — an item-only key let it erase the replacement
    /// derive's marker before the generation check rejected it, re-triggering the
    /// very walk the marker guards against.
    pub pending: HashSet<(u64, usize)>,
}

impl Default for Thumbs {
    fn default() -> Self {
        Thumbs {
            cache: ThumbCache::new(THUMB_BUDGET_BYTES),
            enabled: false,
            failed: HashSet::new(),
            follow: FollowState::default(),
            pending_scroll: None,
            viewport: None,
            tx: None,
            rx: None,
            in_flight: 0,
            pending: HashSet::new(),
        }
    }
}

impl Thumbs {
    /// First panel open: spawn the derive thread and start capturing. Idempotent.
    pub fn enable(&mut self) {
        if self.enabled {
            return;
        }
        self.enabled = true;
        let (tx, job_rx) = sync_channel::<DeriveJob>(DERIVE_QUEUE);
        let (res_tx, rx): (Sender<DeriveResult>, Receiver<DeriveResult>) =
            std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("thumb-derive".into())
            .spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    if let Some(thumb) = pb_decode::derive_thumbnail(job.img, THUMB_EDGE) {
                        if res_tx
                            .send(DeriveResult {
                                deck_gen: job.deck_gen,
                                item: job.item,
                                thumb,
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            })
            .expect("spawn thumb-derive thread");
        self.tx = Some(tx);
        self.rx = Some(rx);
    }

    /// Whether a derive is in flight (keeps the host pump polling).
    pub fn working(&self) -> bool {
        self.in_flight > 0
    }

    /// The current demand window: the shell-reported viewport when it has
    /// reported one, else centered on the current item.
    pub fn demand(&self, current: usize) -> ThumbDemand {
        match self.viewport {
            Some((visible, overscan)) => ThumbDemand {
                visible,
                overscan,
                current,
                warm: THUMB_WARM,
            },
            None => ThumbDemand::centered(current, THUMB_WARM),
        }
    }

    /// Offer a decoded image for thumb derivation — the T0 post-upload handoff
    /// (also the ingest for T1 Thumb-purpose pool results, which are already
    /// small; the derive thread's resize is then a no-op and only the color
    /// bake runs). O(1) on the caller: a bounded `try_send`; full ⇒ dropped.
    pub fn offer(&mut self, item: usize, img: DecodedImage) {
        if !self.enabled {
            return;
        }
        // A resident Full-tier thumb never regresses; skip the queue slot. A
        // preview-tier entry is worth upgrading when a real decode comes through.
        if self.cache.tier(item) == Some(ThumbTier::Full) && !img.is_preview {
            // Re-deriving an identical full is wasted work UNLESS pixels changed
            // (rotation-save re-decode) — those arrive after a cache clear, so
            // the tier check is safe here.
            return;
        }
        if img.format == PixelFormat::Nv12 {
            return;
        }
        let Some(tx) = &self.tx else { return };
        let gen = self.cache.deck_gen();
        let job = DeriveJob {
            deck_gen: gen,
            item,
            img,
        };
        if tx.try_send(job).is_ok() {
            self.in_flight += 1;
            self.pending.insert((gen, item));
        }
    }

    /// Whether `item` has an accepted offer in the derive queue **for the current
    /// generation** — the fill planner's "already on its way" check. A stale
    /// generation's marker never counts (its result will be rejected on landing).
    pub fn pending_now(&self, item: usize) -> bool {
        self.pending.contains(&(self.cache.deck_gen(), item))
    }

    /// Drain finished derives into the cache. Returns true when anything landed
    /// (the caller re-signals the shell). Stale deck generations are discarded —
    /// a result derived before a rebuild can never install under a new index.
    pub fn poll(&mut self, current: usize) -> bool {
        let Some(rx) = &self.rx else { return false };
        let mut changed = false;
        let mut landed = Vec::new();
        while let Ok(r) = rx.try_recv() {
            self.in_flight = self.in_flight.saturating_sub(1);
            // Generation-scoped retirement (#119, Codex r2 h2): a late stale result
            // retires only ITS OWN generation's marker, never the replacement's.
            self.pending.remove(&(r.deck_gen, r.item));
            landed.push(r);
        }
        let demand = self.demand(current);
        for r in landed {
            if r.deck_gen != self.cache.deck_gen() {
                continue; // pre-rebuild pixels — never under a new index
            }
            let tier = if r.thumb.is_preview {
                ThumbTier::Preview
            } else {
                ThumbTier::Full
            };
            let bytes = r.thumb.pixels.len() as u64;
            let px = ThumbPixels {
                rgba: r.thumb.pixels,
                orig_w: r.thumb.orig_width,
                orig_h: r.thumb.orig_height,
                codec: r.thumb.codec,
            };
            if self.cache.insert(
                r.item,
                tier,
                r.thumb.width,
                r.thumb.height,
                bytes,
                px,
                &demand,
            ) == InsertOutcome::Stored
            {
                changed = true;
            }
        }
        changed
    }

    /// Deck rebuilt / item deleted: indices are reassigned — drop everything
    /// (the v1 rule; in-flight derives are fenced by the deck generation).
    /// The generation bump itself is [`invalidate_content`](Self::invalidate_content)'s
    /// job (its caller runs on every content boundary); this adds the view-state
    /// reset that only makes sense when indices were reassigned.
    pub fn clear_deck(&mut self) {
        self.invalidate_content();
        self.viewport = None;
        self.pending_scroll = None;
    }

    /// A content boundary (#119): pixels behind one or more indices changed — a
    /// save-rotation as much as a deck rebuild. Advance the derive fence (the cache
    /// generation) so in-flight derives from pre-change pixels are rejected on
    /// landing, and drop the tiles so the strip re-derives. Deliberately preserves
    /// the strip's viewport / follow / pending-scroll state: a save-rotation must
    /// not yank the panel (correctness over cache retention — a wrong-pixels tile
    /// is a lie; a refill is cheap and rare).
    pub fn invalidate_content(&mut self) {
        self.cache.clear();
        self.failed.clear();
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(w: u32, h: u32) -> DecodedImage {
        DecodedImage {
            width: w,
            height: h,
            orig_width: w,
            orig_height: h,
            codec: "test",
            format: PixelFormat::Rgba8,
            pixels: vec![128; (w * h * 4) as usize],
            is_preview: false,
            color: pb_decode::ColorTransform::srgb(),
            peak: 1.0,
            animated: None,
        }
    }

    fn poll_until(t: &mut Thumbs, current: usize) -> bool {
        for _ in 0..200 {
            if t.poll(current) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn disabled_thumbs_cost_nothing_and_capture_nothing() {
        let mut t = Thumbs::default();
        t.offer(0, img(100, 100));
        assert!(!t.working());
        assert!(!t.poll(0));
        assert_eq!(t.cache.len(), 0);
    }

    #[test]
    fn offer_derives_off_thread_and_lands_in_cache() {
        let mut t = Thumbs::default();
        t.enable();
        t.offer(3, img(2048, 1024));
        assert!(poll_until(&mut t, 3), "derive lands");
        let e = t.cache.get(3).expect("cached");
        assert_eq!(e.tier, ThumbTier::Full);
        assert_eq!((e.w, e.h), (512, 256));
        assert_eq!(e.payload.orig_w, 2048, "true source dims preserved");
        assert_eq!(e.bytes, 512 * 256 * 4);
    }

    #[test]
    fn stale_deck_generation_is_discarded() {
        let mut t = Thumbs::default();
        t.enable();
        t.offer(3, img(256, 256));
        t.clear_deck(); // rebuild: bump the generation while the derive is in flight
                        // Give the derive time to land, then poll — nothing may install.
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!t.poll(3));
        assert_eq!(t.cache.len(), 0, "pre-rebuild pixels never install");
    }

    #[test]
    fn full_tier_offers_are_not_requeued() {
        let mut t = Thumbs::default();
        t.enable();
        t.offer(1, img(64, 64));
        assert!(poll_until(&mut t, 1));
        let gen0 = t.cache.get(1).unwrap().gen;
        // A second full for the same item is skipped at the offer.
        t.offer(1, img(64, 64));
        std::thread::sleep(std::time::Duration::from_millis(50));
        t.poll(1);
        assert_eq!(t.cache.get(1).unwrap().gen, gen0, "no re-derive");
    }

    /// #119 (Codex r1 f6): a single-item content change (save-rotation) advances the
    /// derive fence — in-flight pre-change pixels are rejected on landing — while the
    /// strip's view state survives (the panel must not get yanked).
    #[test]
    fn invalidate_content_fences_in_flight_derives_but_keeps_view_state() {
        let mut t = Thumbs::default();
        t.enable();
        t.viewport = Some(((0, 5), (0, 8)));
        t.pending_scroll = Some(ScrollTo { item: 2, gen: 7 });
        t.offer(3, img(256, 256));
        t.invalidate_content(); // the rotation saved while the derive was in flight
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!t.poll(3));
        assert_eq!(t.cache.len(), 0, "pre-change pixels never install");
        assert_eq!(
            t.viewport,
            Some(((0, 5), (0, 8))),
            "the strip's viewport survives a content edit"
        );
        assert_eq!(
            t.pending_scroll.map(|c| (c.item, c.gen)),
            Some((2, 7)),
            "an unpulled scroll command survives too — only clear_deck resets view state"
        );
        assert!(!t.pending_now(3), "the old generation's marker is gone");
    }

    /// #119 (Codex r2 h2): `pending` is keyed by (generation, item) — a LATE stale result
    /// retires only its own generation's marker, never the replacement derive's. (An
    /// item-only key let the stale arrival erase the replacement's marker, re-triggering
    /// the poster walk the marker guards against.)
    #[test]
    fn a_late_stale_result_cannot_erase_the_replacement_marker() {
        // The mechanism, pinned structurally on the real removal shape:
        let mut t = Thumbs::default();
        t.pending.insert((0, 9)); // the stale generation's marker
        t.pending.insert((1, 9)); // the replacement's
        t.pending.remove(&(0, 9)); // what `poll` does when the late gen-0 result lands
        assert!(
            t.pending.contains(&(1, 9)),
            "the replacement's marker survives the stale retirement"
        );

        // And end-to-end through the real derive thread: the gen-1 replacement lands
        // even though a stale gen-0 derive for the same item was still in flight.
        let mut t = Thumbs::default();
        t.enable();
        t.offer(3, img(2048, 2048)); // gen-0 derive
        t.invalidate_content(); // gen 1
        t.offer(3, img(64, 64)); // the replacement
        assert!(t.pending_now(3), "the replacement's marker is up");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while t.working() && std::time::Instant::now() < deadline {
            t.poll(3);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            t.cache.tier(3),
            Some(ThumbTier::Full),
            "the replacement landed; the stale result installed nothing"
        );
        assert!(!t.pending_now(3), "and retired its own marker");
    }

    #[test]
    fn preview_offers_upgrade_when_the_full_arrives() {
        let mut t = Thumbs::default();
        t.enable();
        let mut prev = img(160, 120);
        prev.is_preview = true;
        t.offer(2, prev);
        assert!(poll_until(&mut t, 2));
        assert_eq!(t.cache.tier(2), Some(ThumbTier::Preview));
        t.offer(2, img(1024, 768));
        assert!(poll_until(&mut t, 2));
        assert_eq!(t.cache.tier(2), Some(ThumbTier::Full));
    }
}
