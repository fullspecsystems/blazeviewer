# macOS archive-video posters — Swift poster gen, ring-integrated

**Status:** **DONE** (2026-07-12) — owner-verified posters + prefetch. Built on the macOS
archive-video *playback* spike (`PlayVideoBytes` + `ArchiveVideoLoader`, 90485072). Relates
to tasks #30 (archive viewing) and #79 (video).

Shipped: Phase 1 (current-item poster, ring-integrated, retention), Phase 2 (prefetch window,
off-thread reads), Phase 3 (off-thread *playback* byte read; brightness-walk black-lead-in
avoidance; archive-video codec/fps/duration/audio in the inspector via a Swift AVFoundation
probe → `archive_video_meta_ready`). Poster cancellation was intentionally skipped — a late
poster still lands in the ring and is used unless the item was already evicted. Remaining
nice-to-haves: MKV/WebM stay on the placeholder (no AVFoundation handler); a mac no-trace
integration test for archive-video viewing (the Rust path adds no `fs::write`; poster gen is
Swift-side).

## Problem

Video *playback* from ZIP/7z archives works on macOS (owner-verified on a solid 7z), but
an archive video shows the **flat placeholder tile** while browsing — no poster/preview
before you press `P`. This is an image viewer; a video with no preview frame defeats half
the point of the feature.

Loose-file videos already get real posters on macOS (`pb_decode::av_poster::decode_video_poster`
→ the livephoto `AVAssetReader` walk over an `AVURLAsset(fileURL:)` — first non-black frame,
correct rotation/color). The gap is **narrow and specific to archive entries**: an archive
entry has no file URL, and **AVFoundation has no AVAsset-from-bytes API**. The only RAM route
to an `AVAsset` over in-memory bytes is an `AVAssetResourceLoaderDelegate`.

Windows posters archive videos from the entry's in-RAM bytes through the same Media
Foundation reader (`decode_video_poster_input` + `mem_istream`, `engine.rs` `#[cfg(windows)]`).
macOS/Linux currently fall through to `video_placeholder`.

## Decision — Route B: Swift generates the poster, feeds pixels into the resident ring

Two routes were weighed:

- **Route A (Rust delegate):** implement `AVAssetResourceLoaderDelegate` via the ObjC runtime
  *in Rust* so `decode_video_poster` works from bytes in the decode pool. Reuses the Rust
  brightness-walk/fit and would match Windows. **Rejected:** requires ~200 lines of novel,
  crash-prone objc-runtime FFI (runtime class creation + method IMPs — no precedent in the
  tree), plus `AVAssetReader` has finicky *synchronous* track-loading when fed a
  resource-loader asset. It also **duplicates the hard part** — the resource-loader delegate —
  which already exists and works in Swift (`ArchiveVideoLoader`, shipped for playback).

- **Route B (Swift gen + ring feedback):** CHOSEN. Reuse the already-working Swift
  `ArchiveVideoLoader`; grab a frame with `AVAssetImageGenerator` (robust high-level API that
  handles async track-loading + `appliesPreferredTrackTransform` orientation); convert to
  RGBA8; hand the pixels back to the core, which ingests them into the **resident ring** as a
  synthetic decode `Outcome` keyed by item.

**Why B is right for macOS going forward** (the same story as the native-AVPlayer decision):
the macOS archive-video pipeline already lives in Swift; the poster belongs there too. B keeps
the pipeline unified in one language, uses robust first-party AVFoundation, avoids the tree's
riskiest-ever code, and reuses the piece that was actually hard to get right. The only
behavioral difference from A is *where the frame decodes* (Swift vs the Rust pool) — invisible
to caching/prefetch/retention.

**Retention + prefetch are NOT sacrificed.** The key design point: the poster lands in the
**resident ring** (the same GPU-texture cache photos use — `drain_results` → `ResidentRing`,
`slot_for(item)` hits). Once it's there:
- **Retention:** revisiting the item is a ring hit — instant, no re-decode — until normal
  eviction, exactly like a photo.
- **Prefetch:** the prefetch scheduler's direction-biased target window already lists upcoming
  items; archive-video targets route to a Swift poster request instead of a pool placeholder,
  and the results land in the ring identically. So a folder of archive videos pre-caches ahead
  like a folder of photos.

## Architecture / data flow

```
prefetch scheduler / current-item decode
  │  target item = macOS archive video (no source.path, source.bytes available)
  ▼
[core] read entry bytes off-thread (RAM-only, never to disk) → stash keyed by (item, request_id)
  │  emit CoreEffect::RequestVideoPoster { item, request_id, name }   (+ fit box)
  ▼
[swift] take_pending_poster_bytes(request_id) → ArchiveVideoLoader(data,name)
  │  AVAssetImageGenerator(asset), appliesPreferredTrackTransform=true,
  │  copyCGImage at a small offset (avoid black lead-in) → RGBA8 (fit to the box)
  │  callback: video_poster_ready(item, request_id, w, h, rgba)
  ▼
[core] wrap as a decode Outcome for (item, epoch) → drain_results uploads into ResidentRing
  │  retention (slot_for hit on revisit) + prefetch (window targets) both automatic
```

## Key seams

- **Outcome ingestion** (`app_core_impl.rs` `drain_results`, `pending_uploads`, `self.results`,
  `ResidentRing`, `slot_for`, `epoch`) — a Swift poster result becomes an `Outcome { key:{item,
  epoch}, result: Ok(DecodedImage) }` pushed into `pending_uploads` (drained next tick). Reuses
  the *entire* photo upload/eviction path.
- **Bytes handoff** — the `take_pending_video_bytes` stash-pull pattern (shipped for
  `PlayVideoBytes`), generalized to a per-request-id map so multiple prefetch posters can be in
  flight. Reads happen **off the event loop** (a large ZIP entry inflates; the playback spike's
  synchronous read is a known follow-up and applies here too).
- **FFI**: new `CoreEffect::RequestVideoPoster` + `CoreEffectFfi`; `take_pending_poster_bytes`
  accessor; Swift→core `video_poster_ready` callback (mirrors `native_video_seek_completed`).
- **Swift poster gen** — `ArchiveVideoLoader` (reused) + `AVAssetImageGenerator`; a new small
  helper converts the `CGImage` to RGBA8 fitted to the requested box.

## Phasing

1. **Retention (current item):** core requests a Swift poster for the *displayed* archive
   video; Swift generates it; core ingests into the ring. Proves the ring round-trip end to
   end (a re-visit is a ring hit, not a re-fetch). Gate: owner sees a real poster replace the
   placeholder, and it sticks on navigate-away-and-back.
2. **Prefetch window:** route archive-video targets in the prefetch window to poster requests
   (per-request-id bytes, direction-biased). A folder of archive videos pre-caches ahead.
3. **Polish:** off-thread byte reads (shared with the playback-spike follow-up); black-lead-in
   avoidance (offset or a cheap brightness check); cancellation when the target leaves the
   window; MKV/WebM (no AVFoundation handler) stay on the placeholder.

## Privacy

RAM-only end to end — archive bytes are never written to disk (no-trace guarantee,
`CLAUDE.md`). The poster is decoded from the in-RAM `AVAssetResourceLoaderDelegate` and lives
only in the resident ring, dropped on exit like every other cache.

## Risks / open questions

- **`AVAssetImageGenerator` black lead-in:** grabbing at t=0 can be black. Start with a small
  offset (~0.5 s clamped to duration); escalate to a brightness check if needed (loose-file
  posters already do a brightness walk — parity is nice-to-have, not required).
- **In-flight coordination:** multiple prefetch posters need per-request-id byte stashes and
  cancellation when a target scrolls out of the window; keep the map bounded.
- **Epoch/geometry:** stamp the request with the current epoch so a stale poster (viewport
  resized mid-decode) is dropped by `drain_results` like any stale pool result.
- **Fit:** Swift must fit the CGImage to the decode-fit box so ring budget + `set_image`
  dimensions match the loose-file path.
