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

use pb_source::ItemSource;

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
/// `deck_gen` and `catalog_gen` answer **different questions and must not be the same
/// number** — passing one value for both was a real defect (see [`crate::app_core::AppCore`]'s
/// `catalog_seq`). `deck_gen` guards the *result* ("is index `item` still this file?");
/// `catalog_gen` stamps the *ids* ("which catalog is this `local_id` from?"). A deck
/// generation shared by every file in a folder made the second guard unable to fire.
pub fn spawn(
    source: &Arc<dyn ItemSource>,
    item: usize,
    deck_gen: u64,
    catalog_gen: u64,
    identity: String,
) -> DetailsProbe {
    let source = Arc::clone(source);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(probe_job(source.as_ref(), item, catalog_gen));
    });
    DetailsProbe {
        gen: deck_gen,
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
pub fn probe_job(source: &dyn ItemSource, item: usize, generation: u64) -> ItemDetails {
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
    #[cfg_attr(
        not(any(windows, target_os = "macos", all(unix, feature = "ffvideo"))),
        allow(unused_mut)
    )]
    let mut dovi_incompatible = false;

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
        // Dolby Vision, stated honestly (macos-video-smoothness §2): compat-id 0
        // (Profile 5) cannot show correct color without RPU reshaping — say so
        // here and let playback warn once; other profiles play their base layer.
        if let Some(dovi) = info.dovi {
            fields.push(("Dolby Vision".into(), dovi_row_value(&dovi)));
            dovi_incompatible = dovi.base_layer_incompatible();
        }
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

        // Windows (task #100): Media Foundation gave the video facts above — keep them,
        // they come from the same reader that decodes the poster, so rotation/color stay
        // identical to it by construction. But MF's catalog is the weakest of the three
        // backends: its media sources do not model subtitle tracks *at all* (a source's
        // presentation descriptor reports 3 streams for a file holding 7), and it exposes
        // no authored dispositions or named channel layouts. FFmpeg reads the container's
        // real stream table, so where it can open the file its catalog supersedes.
        //
        // MF's catalog stays as the fallback for anything FFmpeg can't open — it is now
        // the second-choice Windows backend, not the only one. The catalog names its own
        // `backend`, so the panel stays honest about which one spoke.
        // Catalog only — `ff_probe_video_details` would also build a VideoStreamInfo,
        // which needs a video decoder the demuxers-only Windows FFmpeg does not have
        // (it would fail, and the supersede would silently never happen). MF already
        // gave us the facts; FFmpeg is asked for exactly the part MF cannot answer.
        #[cfg(all(windows, feature = "ffprobe"))]
        {
            let input = crate::video::VideoInput::Path(path.to_path_buf());
            if let Ok(tracks) = pb_decode::ff_probe_tracks(&input, generation) {
                media = Some(tracks);
            }
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
            // Linux/macOS: FFmpeg owns both the facts and the catalog.
            #[cfg(all(feature = "ffvideo", not(windows)))]
            if let Ok(probe) = pb_decode::ff_probe_video_details(&input, generation) {
                fill_rows(&probe.video);
                media = Some(probe.tracks);
            }
            // Windows: Media Foundation reads the entry through its in-RAM byte stream —
            // the same reader the filesystem path uses, so an archived video gets the
            // same facts (98.5) as a loose one.
            #[cfg(windows)]
            if let Ok(probe) = pb_decode::probe_video_details_input(&input, generation) {
                fill_rows(&probe.video);
                media = Some(probe.tracks);
            }
            // ...and the catalog goes to FFmpeg exactly as on the filesystem path above
            // (task #100), so archive parity holds on the thing 98.7 exists to guarantee:
            // a video in a ZIP reports what the same file reports loose. Without this the
            // two would now disagree about subtitles.
            #[cfg(all(windows, feature = "ffprobe"))]
            if let Ok(tracks) = pb_decode::ff_probe_tracks(&input, generation) {
                media = Some(tracks);
            }
        }
    }
    let _ = generation; // unused where no platform probe compiles in

    // --- sidecar subtitles (task #90.1) -----------------------------------------------
    //
    // Files beside the video (`movie.en.forced.srt`) are subtitle tracks too, and the user
    // should not have to care that one lives inside the container and one next to it. They
    // are appended after the container's own streams, sharing the catalog's id namespace.
    //
    // Only when there *is* a catalog: with no backend able to describe the container, the
    // subtitle set's completeness is a statement about the backend, and appending to it
    // would quietly turn "we couldn't enumerate" into a half-truth.
    if let Some(catalog) = media.as_mut() {
        let found = crate::sidecar::discover(source, item);
        if !found.is_empty() {
            // Continue the id sequence past every track the backend already minted.
            let mut next_local_id = catalog
                .tracks()
                .map(|t| t.id.local_id + 1)
                .max()
                .unwrap_or(0);
            for (track, locator) in
                crate::sidecar::sidecar_tracks(&found, generation, &mut next_local_id)
            {
                let local_id = track.id.local_id;
                catalog.subtitles.tracks.push(track);
                catalog.set_locator(local_id, locator);
            }
            // The set now describes container streams *and* sidecars. Its completeness is
            // still the backend's word about the container — a backend that could not
            // enumerate does not become `Complete` just because a .srt turned up beside it.
            catalog.subtitles.total =
                Some(catalog.subtitles.tracks.len().min(u16::MAX as usize) as u16);
        }
    }

    ItemDetails {
        size,
        fields,
        media,
        has_audio,
        probe_state: ProbeState::Ready,
        dovi_incompatible,
    }
}

/// The Details row's value for a Dolby Vision stream — names the profile and what
/// the base layer amounts to on a renderer without RPU reshaping.
#[cfg(any(windows, target_os = "macos", all(unix, feature = "ffvideo")))]
fn dovi_row_value(dovi: &pb_decode::DoviSummary) -> String {
    let base = match dovi.bl_compat_id {
        0 => "colors unsupported",
        1 => "HDR10-compatible base layer",
        2 => "SDR-compatible base layer",
        4 => "HLG-compatible base layer",
        _ => "base-layer compatible",
    };
    format!("Profile {} — {}", dovi.profile, base)
}

#[cfg(all(
    test,
    any(windows, target_os = "macos", all(unix, feature = "ffvideo"))
))]
mod dovi_row_tests {
    use super::*;

    /// The DoVi Details row names the profile and states the base layer honestly
    /// (macos-video-smoothness §2) — and only compat-id 0 flags the warning.
    #[test]
    fn dovi_rows_name_the_profile_and_base_layer() {
        let p5 = pb_decode::DoviSummary {
            profile: 5,
            bl_compat_id: 0,
        };
        assert!(p5.base_layer_incompatible());
        assert_eq!(dovi_row_value(&p5), "Profile 5 — colors unsupported");

        let p8 = pb_decode::DoviSummary {
            profile: 8,
            bl_compat_id: 1,
        };
        assert!(!p8.base_layer_incompatible());
        assert_eq!(
            dovi_row_value(&p8),
            "Profile 8 — HDR10-compatible base layer"
        );

        for compat in [2u8, 4] {
            let d = pb_decode::DoviSummary {
                profile: 8,
                bl_compat_id: compat,
            };
            assert!(
                !d.base_layer_incompatible(),
                "compat {compat} degrades cleanly"
            );
        }
    }
}

/// Windows track-catalog wiring (task #100).
///
/// The interesting assertion is *which backend won*: Media Foundation keeps the video
/// facts, but its catalog must be superseded by FFmpeg's, because MF cannot enumerate
/// subtitle tracks at all. Compiling is not evidence of that — only the resolved catalog
/// is. Runs only where FFmpeg is actually linked.
#[cfg(all(test, windows, feature = "ffprobe"))]
mod windows_ffprobe_tests {
    use super::*;
    use pb_decode::tracks::{MediaBackend, TrackCompleteness};
    use pb_source::FsSource;

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../pb-decode/tests/fixtures/video")
            .join(name)
    }

    #[test]
    fn windows_details_take_the_ffmpeg_catalog_and_the_mf_video_facts() {
        let src = FsSource::new(vec![fixture("multitrack.mkv")]);
        let d = probe_job(&src, 0, 7);

        // The catalog is FFmpeg's — MF's would carry no subtitles at all.
        let cat = d.media.expect("a video must produce a catalog");
        assert_eq!(cat.backend, MediaBackend::FFmpeg);
        assert_eq!(cat.generation, 7);
        assert_eq!(cat.subtitles.completeness, TrackCompleteness::Complete);
        assert_eq!(cat.subtitles.total, Some(4), "MF reports 0 of these 4");
        assert_eq!(cat.audio.total, Some(2));
        // The dispositions + named layout MF cannot see.
        assert!(cat.audio.tracks[0].flags.default);
        assert_eq!(
            cat.audio.tracks[1]
                .audio
                .as_ref()
                .unwrap()
                .layout
                .as_deref(),
            Some("5.1(side)")
        );

        // ...while the video facts still came from Media Foundation's reader — the same
        // one that decodes the poster, which is what keeps rotation/color identical to it.
        assert!(d.has_audio == Some(true));
        let codec = d
            .fields
            .iter()
            .find(|(k, _)| k == "Video codec")
            .map(|(_, v)| v.clone())
            .expect("MF fills the video rows");
        assert_eq!(codec, "H.264");
    }

    /// Archive parity (98.7's whole point): a video inside a ZIP must report what the
    /// same file reports loose — including the subtitles FFmpeg adds. Without the
    /// archive branch's own FFmpeg supersede, the two would silently disagree.
    #[test]
    fn an_archived_video_reports_the_same_tracks_as_the_loose_file() {
        use std::io::Write;

        let loose = probe_job(&FsSource::new(vec![fixture("multitrack.mkv")]), 0, 1)
            .media
            .expect("loose catalog");

        let zip_path = std::env::temp_dir().join(format!("pb_md_ff_{}.zip", std::process::id()));
        {
            let f = std::fs::File::create(&zip_path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            zw.start_file("clip.mkv", zip::write::SimpleFileOptions::default())
                .unwrap();
            zw.write_all(&std::fs::read(fixture("multitrack.mkv")).unwrap())
                .unwrap();
            zw.finish().unwrap();
        }
        let src =
            pb_source::ZipSource::open(&zip_path, None, |ext| ext == "mkv").expect("open zip");
        let archived = probe_job(&src, 0, 1).media.expect("archived catalog");
        let _ = std::fs::remove_file(&zip_path);

        assert_eq!(archived.backend, loose.backend);
        assert_eq!(archived.subtitles.total, loose.subtitles.total);
        assert_eq!(archived.audio.total, loose.audio.total);
    }
}
