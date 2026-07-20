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
