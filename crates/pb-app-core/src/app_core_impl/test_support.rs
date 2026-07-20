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

/// A fake archive source: named entries with no fs paths and a container —
/// exactly what a Zip/SevenZSource looks like to the core, without disk.
pub(super) struct FakeArchive {
    pub(super) names: Vec<String>,
    pub(super) container: PathBuf,
}

impl ItemSource for FakeArchive {
    fn len(&self) -> usize {
        self.names.len()
    }
    fn name(&self, i: usize) -> &str {
        self.names.get(i).map(String::as_str).unwrap_or("")
    }
    fn container(&self) -> Option<&Path> {
        Some(&self.container)
    }
    fn bytes(&self, i: usize) -> std::io::Result<Vec<u8>> {
        self.names
            .get(i)
            .map(|_| vec![0u8])
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "oob"))
    }
}

/// A `Renderer` double whose uploads succeed and whose `derive_fit` always works —
/// the shape a real GPU gives when a mipped Original is resident. `device`/`queue`
/// are never reached headless.
pub(super) struct DeriveOk;

impl pb_render::Renderer for DeriveOk {
    fn resize(&mut self, _: u32, _: u32) {}
    fn set_image(
        &mut self,
        _: &[u8],
        _: u32,
        _: u32,
        _: pb_render::ColorTransform,
        _: bool,
        _: f32,
    ) {
    }
    fn clear_image(&mut self) {}
    fn set_view(&mut self, _: pb_render::ViewTransform) {}
    fn set_overlay(&mut self, _: Option<(&[u8], u32, u32)>, _: u32, _: u32) {}
    fn set_info_line(&mut self, _: Option<(&[u8], u32, u32)>, _: u32, _: pb_render::HAlign) {}
    fn reserve_ring(&mut self, _: usize, _: u32, _: u32) {}
    #[allow(clippy::too_many_arguments)]
    fn upload_slot(
        &mut self,
        _: usize,
        _: &[u8],
        _: u32,
        _: u32,
        _: pb_render::ColorTransform,
        _: bool,
        _: f32,
        _: bool,
    ) -> bool {
        true
    }
    fn derive_fit(
        &mut self,
        _source: pb_render::DeriveSource,
        _dst_slot: usize,
        fit_w: u32,
        fit_h: u32,
        _kernel: u32,
        _mip_bias: i32,
    ) -> Option<pb_render::DerivedFit> {
        Some(pb_render::DerivedFit {
            w: fit_w,
            h: fit_h,
            bytes: fit_w as u64 * fit_h as u64 * 8,
        })
    }
    fn present_slot(&mut self, _: usize) -> bool {
        true
    }
    fn surface_size(&self) -> (u32, u32) {
        (0, 0)
    }
    fn set_letterbox(&mut self, _: [u8; 3]) {}
    fn set_toast(&mut self, _: Option<(&[u8], u32, u32)>, _: u32) {}
    fn set_pie(&mut self, _: Option<(&[u8], u32, u32)>, _: u32) {}
    fn set_tree(&mut self, _: Option<(&[u8], u32, u32)>, _: u32) {}
    fn set_subtitle_overlay(&mut self, _: Option<(&[u8], u32, u32)>, _: f32, _: f32) {}
    fn device(&self) -> &pb_render::wgpu::Device {
        unreachable!("headless test double")
    }
    fn queue(&self) -> &pb_render::wgpu::Queue {
        unreachable!("headless test double")
    }
    fn set_egui_overlay(&mut self, _: Option<&pb_render::wgpu::Texture>) {}
    fn image_size(&self) -> (u32, u32) {
        (0, 0)
    }
    fn set_edr_headroom(&mut self, _: f32) {}
    fn hdr_surface_wants_edr(&self) -> Option<bool> {
        None
    }
    fn poll(&self) {}
    fn render(&mut self) -> Result<bool, pb_render::RenderError> {
        Ok(true)
    }
}

/// A `Renderer` double with working stash semantics: tracks the presented ring slot
/// (so `stash_fit`'s verify-the-presented-slot rule is real) and two stash slots.
#[derive(Default)]
pub(super) struct StashOk {
    pub(super) presented: Option<usize>,
    pub(super) stashed: [bool; 2],
}

impl pb_render::Renderer for StashOk {
    fn resize(&mut self, _: u32, _: u32) {}
    fn set_image(
        &mut self,
        _: &[u8],
        _: u32,
        _: u32,
        _: pb_render::ColorTransform,
        _: bool,
        _: f32,
    ) {
    }
    fn clear_image(&mut self) {}
    fn set_view(&mut self, _: pb_render::ViewTransform) {}
    fn set_overlay(&mut self, _: Option<(&[u8], u32, u32)>, _: u32, _: u32) {}
    fn set_info_line(&mut self, _: Option<(&[u8], u32, u32)>, _: u32, _: pb_render::HAlign) {}
    fn reserve_ring(&mut self, _: usize, _: u32, _: u32) {}
    #[allow(clippy::too_many_arguments)]
    fn upload_slot(
        &mut self,
        _: usize,
        _: &[u8],
        _: u32,
        _: u32,
        _: pb_render::ColorTransform,
        _: bool,
        _: f32,
        _: bool,
    ) -> bool {
        true
    }
    fn present_slot(&mut self, slot: usize) -> bool {
        self.presented = Some(slot);
        true
    }
    fn stash_fit(&mut self, stash_idx: usize, ring_slot: usize) -> bool {
        if stash_idx < 2 && self.presented == Some(ring_slot) {
            self.stashed[stash_idx] = true;
            true
        } else {
            false
        }
    }
    fn present_stash(&mut self, stash_idx: usize) -> bool {
        if stash_idx < 2 && self.stashed[stash_idx] {
            self.presented = None;
            true
        } else {
            false
        }
    }
    fn clear_stash(&mut self, stash_idx: usize) {
        if stash_idx < 2 {
            self.stashed[stash_idx] = false;
        }
    }
    fn surface_size(&self) -> (u32, u32) {
        (0, 0)
    }
    fn set_letterbox(&mut self, _: [u8; 3]) {}
    fn set_toast(&mut self, _: Option<(&[u8], u32, u32)>, _: u32) {}
    fn set_pie(&mut self, _: Option<(&[u8], u32, u32)>, _: u32) {}
    fn set_tree(&mut self, _: Option<(&[u8], u32, u32)>, _: u32) {}
    fn set_subtitle_overlay(&mut self, _: Option<(&[u8], u32, u32)>, _: f32, _: f32) {}
    fn device(&self) -> &pb_render::wgpu::Device {
        unreachable!("headless test double")
    }
    fn queue(&self) -> &pb_render::wgpu::Queue {
        unreachable!("headless test double")
    }
    fn set_egui_overlay(&mut self, _: Option<&pb_render::wgpu::Texture>) {}
    fn image_size(&self) -> (u32, u32) {
        (0, 0)
    }
    fn set_edr_headroom(&mut self, _: f32) {}
    fn hdr_surface_wants_edr(&self) -> Option<bool> {
        None
    }
    fn poll(&self) {}
    fn render(&mut self) -> Result<bool, pb_render::RenderError> {
        Ok(true)
    }
}

