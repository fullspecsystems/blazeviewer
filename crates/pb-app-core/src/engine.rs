//! Engine tuning constants + pure helper functions (NS0 5.5 / Phase B).
//!
//! These migrated verbatim out of the winit shell (`pb-app/src/main.rs`) so the
//! orchestration methods that use them can live on [`AppCore`](crate::AppCore) instead of
//! `impl App`. They are shell-neutral: pure math + the decode entry points (which route
//! through `pb-decode`/`pb-source`, both already core deps). The shell still references a
//! few of these (the ones it shares) via `pb_app_core::engine::*`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pb_decode::{decode_named_bytes, DecodeError, DecodedImage, FitBox, PixelFormat};
use pb_source::PhotoSource;

use crate::meta::PhotoMeta;
use crate::{Action, Nav};

/// VRAM budget for the resident texture ring (~1.5 GB → ~16–32 fit-size slots on
/// a 7680-wide display, far more on smaller ones). Capacity is clamped to [4, 64].
pub const RING_BUDGET_BYTES: u64 = 1_500_000_000;
/// Max slot uploads performed per `about_to_wait` tick, so a burst of finished
/// decodes can't blow the frame budget.
pub const UPLOADS_PER_TICK: usize = 2;
/// Cap on the **full-resolution** "sharp ring" upgraded around the cursor when
/// parked (`upgrade_set`). The preview window can be much larger (up to the ring's
/// 64-slot capacity on small windows), but holding more than this many *fulls*
/// resident is wasted decode — nobody pause-and-steps two dozen photos before
/// either flying (previews carry that) or stopping. Keeps the on-park decode burst
/// bounded. On a 7680 fullscreen the byte-budgeted capacity (~12–32) binds first.
pub const MAX_FULL_RING: usize = 24;

/// Hold-to-zoom curve: the e-folding zoom rate (per second) ramps from a gentle
/// start (fine tuning) to a fast max over `ZOOM_RAMP_SECS`. Time-based so it's
/// frame-rate independent.
pub const ZOOM_MIN_RATE: f32 = 0.5;
pub const ZOOM_MAX_RATE: f32 = 2.5;
pub const ZOOM_RAMP_SECS: f32 = 0.7;

/// Hold-to-pan curve: pan speed (px/sec) ramps from a gentle start to a fast max
/// over `PAN_RAMP_SECS`. Time-based, same shape as zoom (per the owner's note).
pub const PAN_MIN_SPEED: f32 = 450.0;
pub const PAN_MAX_SPEED: f32 = 3200.0;
pub const PAN_RAMP_SECS: f32 = 0.7;

/// Repeat interval for the held frame-step scrub (`,`/`.`), after the initial tap
/// delay (`initial_delay`). ~14 fps — quick enough to scrub, slow enough to read (#37).
pub const FRAME_STEP_REPEAT: Duration = Duration::from_millis(70);

/// How long the user must rest on an animated still before we eagerly decode the whole
/// sequence in the background (so a slow WebP/AVIF plays instantly on `P`). Long enough
/// that tapping straight through a folder of animations never kicks a decode (#37).
pub const EAGER_PREP_DELAY: Duration = Duration::from_millis(250);

/// Cap on the Live Photo motion's long edge when decoding its `.mov` (task #38). The
/// motion is a brief preview, not a pixel-peeping asset, so a ~1440px cap keeps the
/// whole pre-decoded RGBA sequence's RAM bounded (~0.5 GB worst case) without a visible
/// quality cost. Also clamped to the display fit, so a small window decodes smaller.
#[cfg(target_os = "macos")]
pub const MOTION_MAX_LONG_EDGE: u32 = 1440;

/// Max displayed characters for an EXIF value; longer ones are truncated so a
/// single field can't blow out the panel width.
pub const EXIF_VALUE_MAX: usize = 72;

/// "Not-ready" loading-pie tuning (the top-right affordance shown while the next
/// photo is still decoding). The fill is a deliberate "honest-ish" fake: there is
/// no true decode progress, so it eases asymptotically toward — but never reaches
/// — full, on a time constant self-calibrated to how long misses usually take.
/// Only appears once a wait outlasts `PIE_SHOW_DELAY`, so fast hits never flash it.
pub const PIE_SHOW_DELAY: f32 = 0.12; // s a wait must persist before the pie appears
pub const PIE_TAU_MIN: f32 = 0.06; // s floor on the fill time constant
pub const PIE_FILL_CAP: f32 = 0.93; // the wedge never quite completes (the "lie")
pub const PIE_FINISH_FADE: f32 = 0.18; // s to snap-to-full then fade once ready
pub const PIE_GLOW_DUR: f32 = 0.30; // s the keypress brighten-pulse decays over
pub const PIE_EWMA_ALPHA: f32 = 0.30; // weight of the latest wait in the time estimate
pub const PIE_DIAMETER: f32 = 46.0; // logical px (scaled by the display factor)
pub const PIE_MARGIN: f32 = 24.0; // logical px in from the top-right corner

/// Ring capacity from the per-slot byte size and the VRAM budget. Full-res
/// (Original) slots are several times bigger than fit slots, so the prefetch
/// window is correspondingly smaller — but still resident and async.
pub fn ring_capacity(slot_bytes: u64) -> usize {
    ((RING_BUDGET_BYTES / slot_bytes.max(1)) as usize).clamp(4, 64)
}

/// Split the ring into an ahead/behind prefetch window (the current item, always
/// resident, takes the remaining slot). Biased forward; a few behind so reversing
/// stays cheap.
pub fn window_for_capacity(cap: usize) -> (usize, usize) {
    let usable = cap.saturating_sub(1);
    let ahead = (usable * 4 / 5).max(1);
    let behind = usable.saturating_sub(ahead);
    (ahead, behind)
}

/// Translate the decoder's color transform into the renderer's (identical fields,
/// distinct crate types so neither crate depends on the other).
pub fn render_color(c: &pb_decode::ColorTransform) -> pb_render::ColorTransform {
    pb_render::ColorTransform {
        matrix: c.matrix,
        trc: c.trc,
        enabled: c.enabled,
    }
}

/// Whether a decoded image is HDR (scene-linear fp16 → the renderer's HDR path).
pub fn is_hdr(img: &DecodedImage) -> bool {
    img.format == PixelFormat::Rgba16F
}

/// The navigation direction for a nav [`Action`], or `None` for any non-nav action.
/// Bridges the central keymap vocabulary to the engine's `Nav` (used by the press
/// handler and `held_nav`).
pub fn nav_of(action: Action) -> Option<Nav> {
    match action {
        Action::Next => Some(Nav::Forward),
        Action::Prev => Some(Nav::Backward),
        Action::Random => Some(Nav::Random),
        Action::RandomPrev => Some(Nav::RandomPrev),
        _ => None,
    }
}

/// A path shown relative to the scan root (forward-slashed), or its file name if
/// it isn't under the root.
pub fn rel_to_root(path: &Path, root: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(r) => r.to_string_lossy().replace('\\', "/"),
        Err(_) => path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string(),
    }
}

/// The info-panel metadata for `item`: its display path (root-relative for a real
/// file, the archive-relative entry name otherwise) plus the decoded dimensions
/// and codec.
pub fn meta_for(
    source: &dyn PhotoSource,
    item: usize,
    root: &Path,
    img: &DecodedImage,
) -> PhotoMeta {
    let rel = match source.path(item) {
        Some(p) => rel_to_root(p, root),
        None => source.name(item).to_string(),
    };
    PhotoMeta {
        rel,
        w: img.orig_width,
        h: img.orig_height,
        codec: img.codec,
        animated: img.animated,
    }
}

/// Resolve item `item`'s encoded bytes from `source` and decode them to fit. The
/// single decode entry point shared by the off-thread pool and the synchronous
/// (first-frame / resize / copy) paths, so a filesystem photo and a ZIP entry
/// decode through exactly the same routing. All reads are RAM-only.
pub fn decode_item(
    source: &dyn PhotoSource,
    item: usize,
    fit: Option<FitBox>,
    allow_preview: bool,
) -> Result<DecodedImage, DecodeError> {
    let bytes = source
        .bytes(item)
        .map_err(|e| DecodeError::Corrupt(format!("read error: {e}")))?;
    let mut img = decode_named_bytes(source.name(item), &bytes, fit, allow_preview)?;
    // Cheap header sniff so the viewer knows an on-demand animation is available
    // (the ▶ P hint / `P` to play). Off the keypress path — this runs in the decode
    // worker (or the sync first-paint), never on the event loop. The pixels stay the
    // still first frame; only `decode_animation` (on `P`) decodes the whole sequence.
    img.animated = pb_decode::detect_animation(&bytes);
    Ok(img)
}

/// The off-thread decode for an on-demand motion sequence (tasks #37 / #38): a Live
/// Photo's companion `.mov` via AVFoundation when `live` is set, otherwise the item's
/// own bytes as a multi-frame animation. Both return a unified [`pb_decode::Animation`]
/// so playback treats them identically.
pub fn decode_motion_job(
    live: Option<PathBuf>,
    source: &Arc<dyn PhotoSource>,
    item: usize,
    fit: Option<FitBox>,
) -> Result<pb_decode::Animation, DecodeError> {
    #[cfg(target_os = "macos")]
    if let Some(path) = &live {
        // Cap the motion's long edge to the display fit, but never above the RAM ceiling.
        let edge = fit
            .map(|f| f.max_width.max(f.max_height))
            .unwrap_or(MOTION_MAX_LONG_EDGE)
            .min(MOTION_MAX_LONG_EDGE);
        return pb_decode::decode_live_motion(path, edge);
    }
    // Off macOS `live` is always `None` (Live Photos = task #39); acknowledge it there so
    // the parameter isn't flagged unused.
    #[cfg(not(target_os = "macos"))]
    let _ = &live;
    match source.bytes(item) {
        Ok(bytes) => pb_decode::decode_animation(&bytes, fit),
        Err(e) => Err(DecodeError::Corrupt(format!("read error: {e}"))),
    }
}

/// Whether an EXIF `(tag, value)` is a binary blob better left out of the panel —
/// Apple's MakerNote/Padding render as kilobytes of hex, and any value that long
/// is binary noise, not human-readable metadata.
pub fn is_exif_blob(tag: &str, value: &str) -> bool {
    matches!(tag, "MakerNote" | "Padding") || value.len() > 256
}

/// Truncate an over-long EXIF value to `EXIF_VALUE_MAX` characters with an
/// ellipsis (counted in chars, so multibyte values aren't split mid-codepoint).
pub fn truncate_exif_value(value: &str) -> String {
    if value.chars().count() <= EXIF_VALUE_MAX {
        value.to_string()
    } else {
        let mut s: String = value.chars().take(EXIF_VALUE_MAX).collect();
        s.push('…');
        s
    }
}

/// A copy of `rgba` with its alpha channel scaled by `factor` (clamped 0..=1).
pub fn scale_alpha(rgba: &[u8], factor: f32) -> Vec<u8> {
    let f = factor.clamp(0.0, 1.0);
    let mut out = rgba.to_vec();
    for px in out.chunks_exact_mut(4) {
        px[3] = (px[3] as f32 * f).round().clamp(0.0, 255.0) as u8;
    }
    out
}

/// The file name of a path-like source name (strips any directory prefix).
pub fn file_name_of(name: &str) -> String {
    Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
        .to_string()
}

/// Whether physical-px point `(x, y)` lies within `[x0, y0, x1, y1]` (inclusive) — the
/// overlay click hit-test (the scan-count chip today; EXIF copy buttons later).
pub fn point_in_rect([x0, y0, x1, y1]: [f32; 4], x: f32, y: f32) -> bool {
    x >= x0 && x <= x1 && y >= y0 && y <= y1
}

/// The window-title string for a photo: `name (idx/n)`.
pub fn title_for(name: &str, idx: usize, n: usize) -> String {
    format!("{} ({}/{n})", file_name_of(name), idx + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_exif_value_caps_long_values() {
        assert_eq!(truncate_exif_value("1/250 s"), "1/250 s");
        let out = truncate_exif_value(&"a".repeat(100));
        assert_eq!(out.chars().count(), EXIF_VALUE_MAX + 1); // value + ellipsis
        assert!(out.ends_with('…'));
        // Multibyte values are truncated on char boundaries, not bytes.
        let out = truncate_exif_value(&"é".repeat(100));
        assert_eq!(out.chars().count(), EXIF_VALUE_MAX + 1);
    }

    #[test]
    fn window_split_biases_forward_and_reserves_current() {
        let (ahead, behind) = window_for_capacity(11);
        assert_eq!(ahead + behind, 10); // one slot reserved for the current item
        assert!(ahead > behind);
    }

    #[test]
    fn point_in_rect_is_inclusive() {
        assert!(point_in_rect([0.0, 0.0, 10.0, 10.0], 0.0, 10.0));
        assert!(!point_in_rect([0.0, 0.0, 10.0, 10.0], 11.0, 5.0));
    }

    #[test]
    fn scale_alpha_scales_only_alpha() {
        let rgba = [10u8, 20, 30, 200];
        let out = scale_alpha(&rgba, 0.5);
        assert_eq!(&out[..3], &[10, 20, 30]);
        assert_eq!(out[3], 100);
    }
}
