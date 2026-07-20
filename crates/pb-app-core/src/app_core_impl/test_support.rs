//! Shared test fixtures for the `app_core_impl` concern test modules (task #128).
//!
//! The cross-concern fixtures — `test_core` and the deck/photo/residency builders that every
//! concern's tests lean on. Concern-*local* fixtures stay with their concern's `mod tests`;
//! only what more than one concern needs lives here.
//!
//! `pub(super)` == `pub(in crate::app_core_impl)`: visible to `app_core_impl` and every
//! descendant (the parent's `mod tests` and each `<concern>::tests`), no wider. Import these
//! **explicitly and absolutely** from a concern's tests —
//! `use crate::app_core_impl::test_support::{test_core, ...}` — never by glob (plan §5): a
//! glob can silently rebind a generically-named helper, compiling green while testing the
//! wrong thing.

use super::*;
use crate::Viewport;

pub(super) fn test_core() -> AppCore {
    AppCore::headless(Viewport {
        width: 1,
        height: 1,
        scale_factor: 1.0,
    })
}

/// A five-item source named `a.jpg`..`e.jpg` under a folder, for the launch-start tests.
pub(super) fn five_photos() -> Arc<dyn ItemSource> {
    Arc::new(FsSource::new(
        ["a", "b", "c", "d", "e"]
            .iter()
            .map(|n| PathBuf::from(format!("photos/{n}.jpg")))
            .collect(),
    ))
}

pub(super) fn photos_named(names: &[&str]) -> Arc<dyn ItemSource> {
    Arc::new(FsSource::new(
        names
            .iter()
            .map(|n| PathBuf::from(format!("p/{n}")))
            .collect(),
    ))
}

/// Populate the ring so `item` is resident in `rep` at the core's current content gen.
pub(super) fn make_resident(
    core: &mut AppCore,
    item: usize,
    rep: pb_core::Representation,
    keep: &[usize],
) {
    let cg = core.content_gen;
    let res = core
        .ring
        .reserve_bytes(item, cg, rep, 64, keep)
        .expect("a free slot");
    assert!(core.ring.mark_resident(item, res.slot, cg, rep));
}

/// A definitive full-quality decode (`is_preview: false`, sized to the fit) for the
/// #109.4 refused-upload tests.
pub(super) fn rgba_full(w: u32, h: u32, orig_w: u32, orig_h: u32) -> pb_decode::DecodedImage {
    pb_decode::DecodedImage {
        width: w,
        height: h,
        orig_width: orig_w,
        orig_height: orig_h,
        codec: "JPEG",
        format: pb_decode::PixelFormat::Rgba8,
        pixels: vec![0; (w * h * 4) as usize],
        is_preview: false,
        color: pb_decode::ColorTransform::srgb(),
        peak: 1.0,
        animated: None,
        recovered: None,
    }
}

pub(super) fn track(codec: &str, lang: &str) -> pb_decode::MediaTrack {
    pb_decode::MediaTrack {
        id: pb_decode::TrackId {
            catalog_generation: 1,
            local_id: 0,
        },
        kind: pb_decode::TrackKind::Audio,
        language: Some(lang.into()),
        title: None,
        codec_raw: codec.to_ascii_lowercase(),
        codec: codec.into(),
        capability: pb_decode::TrackCapability::Playable,
        flags: pb_decode::TrackFlags::none(),
        audio: Some(pb_decode::AudioFormat {
            channels: 2,
            layout: Some("stereo".into()),
            sample_rate: 48000,
        }),
        external: false,
    }
}

/// A core with a live **Native** backend on item 0 — the macOS sample-buffer /
/// AVPlayer shape, which is what MKV and WebM actually take since Phase 3F.
pub(super) fn core_with_a_native_video() -> AppCore {
    let mut core = test_core();
    let sid = pb_decode::VideoSessionId(7);
    let mut proxy = crate::video_native::NativeVideoProxy::new(0, sid, false);
    proxy.on_state_changed(sid, crate::video::VideoSessionState::Playing);
    core.video = Some(crate::video_native::ActiveVideoBackend::Native(proxy));
    core.displayed_item = Some(0);
    core
}

pub(super) fn text_result(qr: &[&str], lines: &[&str]) -> crate::image_text::ImageText {
    crate::image_text::ImageText {
        qr: qr.iter().map(|s| s.to_string()).collect(),
        lines: lines.iter().map(|s| s.to_string()).collect(),
        ocr_error: None,
    }
}

pub(super) fn clipboard_text_effects(core: &AppCore) -> Vec<(String, Option<String>)> {
    core.effects
        .iter()
        .filter_map(|e| match e {
            contract::CoreEffect::WriteClipboard(contract::ClipboardPayload::Text {
                text,
                toast,
            }) => Some((text.clone(), toast.clone())),
            _ => None,
        })
        .collect()
}

/// Seed the Details cache for `item` with a catalog, as a probe would.
pub(super) fn seed_details(
    core: &mut AppCore,
    item: usize,
    media: Option<pb_decode::MediaTrackCatalog>,
    has_audio: Option<bool>,
) {
    core.exif_cache.insert(
        item,
        crate::app_core::ItemDetails {
            size: 1234,
            fields: vec![("Video codec".into(), "HEVC".into())],
            media,
            has_audio,
            probe_state: crate::media_details::ProbeState::Ready,
            dovi_incompatible: false,
        },
    );
}

pub(super) fn poster_payload(item: usize, fitted: (u32, u32)) -> pb_decode::PosterSelection {
    let img = |w: u32, h: u32| pb_decode::DecodedImage {
        width: w,
        height: h,
        orig_width: 3840,
        orig_height: 2160,
        codec: "HEVC",
        format: pb_decode::PixelFormat::Rgba8,
        pixels: vec![128; (w * h * 4) as usize],
        is_preview: false,
        color: pb_decode::ColorTransform::srgb(),
        peak: 1.0,
        animated: None,
        recovered: None,
    };
    pb_decode::PosterSelection {
        choice: pb_decode::PosterChoice {
            origin_hns: 0,
            relative_hns: item as i64 * 10_000_000,
            native_w: 3840,
            native_h: 2160,
            content_hdr: false,
        },
        fit_img: Some(img(fitted.0, fitted.1)),
        thumb_img: Some(img(64, 36)),
        native: None,
    }
}

/// Sets up a core parked on item 0 displayed as a resident PREVIEW, with a nav key stuck
/// held (the lost-key-up race): `held` claims Space is down, but no release will ever come.
/// `hold_start`/`initial_delay` are pinned so the tick's step-3 advance machinery stays out
/// of the way — the subject under test is the 3b sharpen gate, not hold-to-blaze.
pub(super) fn stuck_preview_core() -> AppCore {
    let mut core = test_core();
    core.source = photos_named(&["a.jpg"]);
    core.playlist = Playlist::new(1, 0).with_cursor(0);
    core.ring = ResidentRing::new(4);
    core.fit = Some(FitBox {
        max_width: 100,
        max_height: 100,
    });
    core.view.mode = ScaleMode::Fit;
    let fit_rep = core.rep_of(pb_core::RepKind::Fit);
    make_resident(&mut core, 0, fit_rep, &[0]);
    core.preview_resident.insert(0);
    core.targets = vec![0];
    core.displayed_item = Some(0);
    core.target_item = Some(0);
    core.mark_resolved(0);
    core.held.insert(PbKey::Space, Action::Next);
    core.hold_start = Some(core.now);
    core.initial_delay = Duration::from_secs(3600);
    core
}
