//! The off-thread **video Details probe** (task #98, phase 2).
//!
//! Opening a container to read its stream table is not something the event loop should
//! ever wait on. It was ~15–25 ms for the basic facts, the track catalog asks the demuxer
//! for more, and neither number is the one that matters: a damaged container, a file on a
//! slow network share, or a codec that makes the OS reader work hard can take
//! *arbitrarily* long. A synchronous probe turns any of those into a visible hitch on the
//! very keypress that opened the Inspector.
//!
//! So the probe runs on a worker and the result is picked up in `tick`, exactly like the
//! OCR scan ([`crate::image_text::TextScan`]) — same shape, same staleness discipline, so
//! there is one async-panel pattern in the core rather than two.
//!
//! **Stills stay synchronous.** They read bytes we generally already have, and their
//! result feeds callers that read it back immediately (`default_describe_prompt`, the HUD
//! `exif_lines`). Only the video branch — the one with an unbounded open behind it — moves.

use std::sync::mpsc::Receiver;
use std::sync::Arc;

use pb_source::PhotoSource;

use crate::app_core::ItemDetails;

/// An in-flight Details probe. Held as a single `Option` on the core: replacing it drops
/// the receiver, the worker's `send` fails, and its thread exits quietly — so replacement
/// *is* cancellation, and no separate request id is needed to tell an old result from a
/// new one (the old one can no longer be received at all).
pub struct DetailsProbe {
    /// The deck generation when the probe was requested. Item indices are reassigned on a
    /// playlist rebuild, so a result minted against an older deck names a *different file*
    /// and must be dropped.
    pub gen: u64,
    pub item: usize,
    /// The item's name when the probe was requested. The generation guards a rebuild; this
    /// guards the subtler case — the deck being re-sourced such that index `item` now
    /// points at another file. Cheap (one `String` per probe, off the hot path) and it
    /// makes "same index, different file" unrepresentable rather than merely unlikely.
    pub identity: String,
    /// Copy the details to the clipboard when they land (the Copy Image Details command
    /// ran while the probe was still in flight). Mirrors
    /// [`crate::image_text::TextScan::copy_when_done`] — a user command must produce the
    /// *complete* copy, not whatever happened to be cached when they pressed it.
    pub copy_when_done: bool,
    pub rx: Receiver<ItemDetails>,
}

/// How far a Details entry's probe got. (There is no `Idle`: an entry only exists once a
/// probe has been started, so an idle state would be unreachable — the *absence* of the
/// cache entry is what "not started" means.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProbeState {
    /// A worker is probing; the panel shows this honestly rather than an empty table.
    #[default]
    Loading,
    /// Done. (Including "done, and the container told us nothing" — the catalog's own
    /// completeness carries *that* distinction, not this.)
    Ready,
    /// The worker died. Distinct from `Ready`-with-nothing so a stuck entry can't sit on
    /// "Reading…" forever.
    Failed,
}

/// Spawn the probe for `item`, returning the handle to poll. The worker owns an `Arc` of
/// the source, so it stays alive even if the deck is rebuilt underneath it.
pub fn spawn(
    source: &Arc<dyn PhotoSource>,
    item: usize,
    gen: u64,
    identity: String,
) -> DetailsProbe {
    let source = Arc::clone(source);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(probe_job(source.as_ref(), item, gen));
    });
    DetailsProbe {
        gen,
        item,
        identity,
        copy_when_done: false,
        rx,
    }
}

/// The whole worker-thread job: stat the file, then probe the container for its basic
/// facts and its track catalog. Never returns an `Err` — a container we can't open yields
/// an entry with no catalog, which the Details rows render as "details unavailable"
/// rather than as a claim about the file.
///
/// `generation` stamps every [`pb_decode::TrackId`] in the catalog, so a `TrackId` minted
/// here can never resolve against the deck that replaced this one.
pub fn probe_job(source: &dyn PhotoSource, item: usize, generation: u64) -> ItemDetails {
    // The panel's file size comes from a stat, or the archive directory's size hint for an
    // entry — never from reading the file.
    let size = source
        .path(item)
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .or_else(|| source.size_hint(item))
        .unwrap_or(0);

    // The Windows (Media Foundation), macOS (AVFoundation), and Linux (FFmpeg, task #84)
    // probes below fill these; on other platforms they stay empty, so `mut` reads as
    // unused — suppress it there rather than drop `mut`.
    #[cfg_attr(
        not(any(windows, target_os = "macos", all(unix, feature = "ffvideo"))),
        allow(unused_mut)
    )]
    let mut fields: Vec<(String, String)> = Vec::new();
    #[cfg_attr(
        not(any(windows, target_os = "macos", all(unix, feature = "ffvideo"))),
        allow(unused_mut)
    )]
    let mut media: Option<pb_decode::MediaTrackCatalog> = None;
    #[cfg_attr(
        not(any(windows, target_os = "macos", all(unix, feature = "ffvideo"))),
        allow(unused_mut)
    )]
    let mut has_audio: Option<bool> = None;

    // One shared row builder so the three platform probes can't drift on copy. The bare
    // `Audio: Yes/No` row it used to add is gone: the real per-track listing (task #98)
    // supersedes it, and `track_rows` reports "No" from the catalog's own completeness
    // rather than from a bool.
    #[cfg(any(windows, target_os = "macos", all(unix, feature = "ffvideo")))]
    let mut fill_rows = |info: &pb_decode::VideoStreamInfo| {
        if let Some(d) = info.duration {
            fields.push(("Duration".into(), crate::video::format_video_duration(d)));
        }
        fields.push(("Video codec".into(), info.codec.to_string()));
        if info.fps > 0.0 {
            fields.push(("Frame rate".into(), format!("{:.2} fps", info.fps)));
        }
        has_audio = Some(info.has_audio);
    };

    #[cfg(any(windows, target_os = "macos"))]
    if let Some(path) = source.path(item) {
        match pb_decode::probe_video_details(path, generation) {
            Ok(probe) => {
                fill_rows(&probe.video);
                media = Some(probe.tracks);
            }
            // macOS + ffvideo (task #84 §8): AVFoundation can't probe the containers the
            // FFmpeg backend plays (MKV/WebM/…) — same fallback split as playback, so the
            // inspector rows appear.
            #[cfg(all(target_os = "macos", feature = "ffvideo"))]
            Err(_) => {
                let input = crate::video::VideoInput::Path(path.to_path_buf());
                if let Ok(probe) = pb_decode::ff_probe_video_details(&input, generation) {
                    fill_rows(&probe.video);
                    media = Some(probe.tracks);
                }
            }
            #[cfg(not(all(target_os = "macos", feature = "ffvideo")))]
            Err(_) => {}
        }
    }
    // Linux (task #84): the FFmpeg probe, path or in-RAM archive bytes alike.
    #[cfg(all(unix, not(target_os = "macos"), feature = "ffvideo"))]
    if let Some(path) = source.path(item) {
        let input = crate::video::VideoInput::Path(path.to_path_buf());
        if let Ok(probe) = pb_decode::ff_probe_video_details(&input, generation) {
            fill_rows(&probe.video);
            media = Some(probe.tracks);
        }
    }

    // --- archive entries (task 98.7) --------------------------------------------------
    //
    // An archive entry has no path, so every probe above skipped it and the panel showed
    // a loose MKV and *the same MKV inside a ZIP* differently. The entry's bytes are the
    // input instead, over the same `VideoInput::Bytes` seam playback already uses.
    //
    // This inflates the entry into RAM, which is exactly why it used to be refused here —
    // but the objection was that it happened **on the event loop**, and 98.6 moved this
    // whole job to a worker. RAM-only and dropped with the probe, so the no-trace
    // guarantee is untouched. (7z entries are already resident: that source is
    // eager-decode-to-RAM.)
    #[cfg(any(windows, feature = "ffvideo"))]
    if media.is_none() && source.path(item).is_none() {
        if let Ok(data) = source.bytes(item) {
            let input = crate::video::VideoInput::Bytes {
                data: std::sync::Arc::new(data),
                // The name carries the real extension, which is what routes the container
                // handler — a byte stream has no URL to sniff.
                name: source.name(item).to_string(),
            };
            #[cfg(feature = "ffvideo")]
            if let Ok(probe) = pb_decode::ff_probe_video_details(&input, generation) {
                fill_rows(&probe.video);
                media = Some(probe.tracks);
            }
            // Windows: Media Foundation reads the entry through its in-RAM byte stream —
            // the same reader the filesystem path uses, so an archived video now gets the
            // same facts *and* the same track catalog (98.5) as a loose one.
            #[cfg(all(windows, not(feature = "ffvideo")))]
            if let Ok(probe) = pb_decode::probe_video_details_input(&input, generation) {
                fill_rows(&probe.video);
                media = Some(probe.tracks);
            }
        }
    }
    let _ = generation; // unused where no platform probe compiles in

    ItemDetails {
        size,
        fields,
        media,
        has_audio,
        probe_state: ProbeState::Ready,
    }
}
