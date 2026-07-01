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
use pb_render::{Rotation, ScaleMode};
use pb_source::PhotoSource;

use crate::meta::PhotoMeta;
use crate::settings::ScaleModePref;
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

/// How long a finished Live Photo lingers on its last motion frame before reverting to
/// the crisp still — "a beat after the video finishes" (task #38).
pub const LIVE_REVERT_DELAY: Duration = Duration::from_millis(450);

/// Cap on the Live Photo motion's long edge when decoding its `.mov` (task #38). The
/// motion is a brief preview, not a pixel-peeping asset, so a ~1440px cap keeps the
/// whole pre-decoded RGBA sequence's RAM bounded (~0.5 GB worst case) without a visible
/// quality cost. Also clamped to the display fit, so a small window decodes smaller.
#[cfg(target_os = "macos")]
pub const MOTION_MAX_LONG_EDGE: u32 = 1440;

/// Max displayed characters for an EXIF value; longer ones are truncated so a
/// single field can't blow out the panel width.
pub const EXIF_VALUE_MAX: usize = 72;

/// Fixed width of the scan status card, in logical px (scaled by the display factor, then
/// clamped to the window). Wide enough for a reasonable current-folder path; longer paths are
/// truncated. Fixed so the card doesn't jitter as the count / live path change width.
pub const SCAN_CARD_WIDTH: f32 = 320.0;

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

/// The playlist cursor after removing item `removed` from a list of `len`, or `None` when the
/// list becomes empty. Stays on the *next* photo (same index) except when the **last** item was
/// removed, where it falls back to the new last index (the **previous** photo). Pure.
pub fn cursor_after_removal(len: usize, removed: usize) -> Option<usize> {
    if len <= 1 {
        return None;
    }
    let new_len = len - 1;
    Some(removed.min(new_len - 1))
}

/// The frame-step direction encoded by an action: `+1` next / `-1` previous / `0`
/// for anything else.
pub fn frame_step_dir(action: Action) -> i32 {
    match action {
        Action::FrameNext => 1,
        Action::FramePrev => -1,
        _ => 0,
    }
}

/// Map the persisted scale-mode preference to the renderer's [`ScaleMode`].
pub fn scale_mode_of(p: ScaleModePref) -> ScaleMode {
    match p {
        ScaleModePref::Fit => ScaleMode::Fit,
        ScaleModePref::Fill => ScaleMode::Fill,
        ScaleModePref::Original => ScaleMode::Original,
    }
}

/// The folder the Open dialog should start in.
///
/// Priority: a user-pinned `fixed` folder (the Settings preference) wins outright. Else,
/// for an **archive** source, the folder that *contains* the archive — never the archive
/// itself: the OS file dialog can't browse inside a `.zip`/`.7z`, and an *encrypted* one
/// errors outright ("Windows cannot open the folder…"). Else the current photo's folder
/// (the scanned folder, then the display root). When all of those are empty — a bare
/// launch with nothing open — fall back to `fallback` (the user's Pictures/home), so the
/// dialog never falls back to *Windows'* own last-folder memory (a privacy trace).
pub fn picker_start_dir(
    fixed: Option<&Path>,
    container: Option<&Path>,
    scan_root: Option<&Path>,
    root: &Path,
    fallback: &Path,
) -> PathBuf {
    let non_empty = |p: Option<&Path>| {
        p.filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
    };
    if let Some(dir) = non_empty(fixed) {
        return dir;
    }
    if let Some(archive) = container {
        return non_empty(archive.parent()).unwrap_or_else(|| fallback.to_path_buf());
    }
    non_empty(scan_root)
        .or_else(|| non_empty(Some(root)))
        .unwrap_or_else(|| fallback.to_path_buf())
}

/// A safe default folder for the Open dialog when nothing else applies (a bare launch):
/// the user's Pictures folder if it exists, else their home, else the current directory.
/// Used so the dialog always opens somewhere real instead of letting Windows fall back to
/// its remembered last folder.
pub fn default_picker_dir() -> PathBuf {
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    if let Some(home) = std::env::var_os(home_var) {
        let home = PathBuf::from(home);
        let pics = home.join("Pictures");
        if pics.is_dir() {
            return pics;
        }
        if home.is_dir() {
            return home;
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// sRGB OETF (scene-linear → sRGB-encoded), matching the present shader's
/// `srgb_oetf` in `pb-render/src/gpu.rs`.
pub fn srgb_oetf(c: f32) -> f32 {
    let x = c.clamp(0.0, 1.0);
    if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// Extended-Reinhard tone-map with white point `lw`, matching the present shader's
/// `reinhard`. `lw = 1` is the identity (faithful SDR); a larger `lw` rolls HDR
/// highlights into the displayable range.
fn reinhard(v: f32, lw: f32) -> f32 {
    let x = v.max(0.0);
    x * (1.0 + x / (lw * lw)) / (1.0 + x)
}

/// Convert a decoded image to a straight-alpha RGBA8 buffer for the clipboard.
///
/// - `Rgba8` is taken as-is (source-encoded sRGB). The DIB clipboard format carries
///   no ICC profile, so a wide-gamut source pastes interpreted as sRGB — a
///   documented v1 limitation.
/// - `Rgba16F` (HDR scene-linear scRGB) is tone-mapped to SDR sRGB8 exactly as the
///   SDR present pass does: extended-Reinhard at the image `peak`, then sRGB-encode.
pub fn to_clipboard_rgba8(img: &DecodedImage) -> Vec<u8> {
    match img.format {
        PixelFormat::Rgba8 => img.pixels.clone(),
        PixelFormat::Rgba16F => {
            let lw = img.peak.max(1.0);
            let px_count = (img.width as usize) * (img.height as usize);
            let mut out = Vec::with_capacity(px_count * 4);
            // 4 half-floats (8 bytes) per pixel, little-endian.
            for px in img.pixels.chunks_exact(8) {
                for ch in 0..3 {
                    let h = half::f16::from_le_bytes([px[ch * 2], px[ch * 2 + 1]]);
                    let v = srgb_oetf(reinhard(h.to_f32(), lw));
                    out.push((v * 255.0 + 0.5) as u8);
                }
                out.push(255); // opaque; HDR sources have no meaningful alpha here
            }
            out
        }
    }
}

/// Rotate a tightly-packed RGBA8 buffer by a 90° quadrant (clockwise), returning the
/// rotated buffer and its new dimensions. `R0` clones unchanged. Used to bake the
/// in-RAM rotation override (the `r` / `Shift+R` overlay transform, which is a GPU
/// transform — not baked into the decoded pixels) into the copied image so the
/// clipboard is WYSIWYG.
pub fn rotate_rgba8(pixels: &[u8], w: u32, h: u32, rot: Rotation) -> (Vec<u8>, u32, u32) {
    if rot == Rotation::R0 {
        return (pixels.to_vec(), w, h);
    }
    let (wu, hu) = (w as usize, h as usize);
    let (new_w, new_h) = if rot.swaps_axes() { (h, w) } else { (w, h) };
    let nwu = new_w as usize;
    let mut out = vec![0u8; wu * hu * 4];
    for sy in 0..hu {
        for sx in 0..wu {
            // Destination pixel coordinates for this source pixel after the turn.
            let (dx, dy) = match rot {
                Rotation::R90 => (hu - 1 - sy, sx),
                Rotation::R180 => (wu - 1 - sx, hu - 1 - sy),
                Rotation::R270 => (sy, wu - 1 - sx),
                Rotation::R0 => unreachable!(),
            };
            let src = (sy * wu + sx) * 4;
            let dst = (dy * nwu + dx) * 4;
            out[dst..dst + 4].copy_from_slice(&pixels[src..src + 4]);
        }
    }
    (out, new_w, new_h)
}

/// The QuickTime motion component paired with a Live Photo still: a sibling file with
/// the **same stem** and a `.mov`/`.qt` extension — Apple's on-disk convention
/// (`IMG_1234.HEIC` + `IMG_1234.MOV`). Filename pairing is fast (one `stat`, no metadata
/// read) and matches the export layout; a content-identifier cross-check to reject a
/// coincidental name collision is a possible refinement (task #38). Returns the motion
/// path if such a sibling exists on disk. Archive entries have no filesystem path, so
/// they never pair here (Live-Photos-in-archives is out of scope for v1).
///
/// macOS-only for now: the motion decoder is AVFoundation, so a Windows build (Live
/// Photos = task #39) has no consumer for a pairing and would flag this unused.
#[cfg(target_os = "macos")]
pub fn companion_motion(still: &Path) -> Option<PathBuf> {
    let dir = still.parent()?;
    let stem = still.file_stem()?;
    for ext in ["mov", "MOV", "qt", "QT"] {
        let mut cand = dir.join(stem);
        cand.set_extension(ext);
        if cand != still && cand.is_file() {
            return Some(cand);
        }
    }
    None
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

    #[test]
    fn remove_middle_keeps_cursor_on_the_next_photo() {
        // [0 1 2 3 4], remove 2 → [0 1 3 4]; cursor stays at 2 (was photo 3, the next).
        assert_eq!(cursor_after_removal(5, 2), Some(2));
        assert_eq!(cursor_after_removal(5, 0), Some(0));
    }

    #[test]
    fn remove_last_falls_back_to_the_previous() {
        // [0 1 2], remove 2 (the last) → [0 1]; cursor → 1 (the new last = previous).
        assert_eq!(cursor_after_removal(3, 2), Some(1));
        assert_eq!(cursor_after_removal(2, 1), Some(0));
    }

    #[test]
    fn remove_only_item_empties_the_playlist() {
        assert_eq!(cursor_after_removal(1, 0), None);
        assert_eq!(cursor_after_removal(0, 0), None);
    }

    #[test]
    fn cursor_stays_in_bounds_of_the_shrunk_list() {
        for len in 1..=10 {
            for removed in 0..len {
                if let Some(c) = cursor_after_removal(len, removed) {
                    assert!(
                        c < len - 1,
                        "cursor {c} must index the shrunk list (len {})",
                        len - 1
                    );
                }
            }
        }
    }

    #[test]
    fn picker_starts_in_the_folder_containing_an_archive() {
        // Archive source: container is the .7z file; the Open dialog must start in its
        // parent folder, never the archive itself (the OS dialog can't browse inside it,
        // and an encrypted one errors). Holds for zip + 7z, encrypted or not.
        let fb = Path::new("fallback");
        let archive = Path::new("photos/trips/spain.7z");
        let got = picker_start_dir(None, Some(archive), None, archive, fb);
        assert_eq!(got, Path::new("photos/trips"));

        let zip = Path::new("albums/2015.zip");
        assert_eq!(
            picker_start_dir(None, Some(zip), None, zip, fb),
            Path::new("albums")
        );
    }

    #[test]
    fn picker_uses_the_scanned_folder_for_a_normal_source() {
        // No archive, no pin: prefer the scanned folder, else the display root.
        let fb = Path::new("fallback");
        let folder = Path::new("photos/trips");
        assert_eq!(
            picker_start_dir(None, None, Some(folder), folder, fb),
            folder
        );

        let root = Path::new("photos");
        assert_eq!(picker_start_dir(None, None, None, root, fb), root);
    }

    #[test]
    fn picker_pinned_folder_wins_over_everything() {
        // A user-pinned folder is used regardless of the current source (incl. archives).
        let fb = Path::new("fallback");
        let pinned = Path::new("D:/AlwaysHere");
        let archive = Path::new("photos/trips/spain.7z");
        assert_eq!(
            picker_start_dir(Some(pinned), Some(archive), None, archive, fb),
            pinned
        );
        assert_eq!(
            picker_start_dir(
                Some(pinned),
                None,
                Some(Path::new("photos")),
                Path::new("photos"),
                fb
            ),
            pinned
        );
    }

    #[test]
    fn picker_falls_back_when_there_is_no_current_folder() {
        // Bare launch: empty scan_root + empty root → the safe fallback, NOT an empty
        // path (which would let Windows surface its own remembered last folder).
        let fb = Path::new("fallback");
        let empty = Path::new("");
        assert_eq!(picker_start_dir(None, None, None, empty, fb), fb);
        assert_eq!(picker_start_dir(None, None, Some(empty), empty, fb), fb);
    }

    /// Live Photo pairing: a still finds its same-stem sibling `.mov`; a still with no
    /// motion clip pairs to nothing. Filename-based (Apple's on-disk convention, #38).
    #[cfg(target_os = "macos")]
    #[test]
    fn companion_motion_pairs_a_same_stem_mov() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("pb_livepair_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir");
        let still = dir.join("IMG_1.heic");
        let motion = dir.join("IMG_1.mov");
        fs::write(&still, b"still").expect("seed");
        fs::write(&motion, b"motion").expect("seed");
        // A still with no companion motion clip pairs to nothing.
        let solo = dir.join("IMG_2.jpg");
        fs::write(&solo, b"solo").expect("seed");

        assert_eq!(companion_motion(&still), Some(motion));
        assert_eq!(companion_motion(&solo), None);

        let _ = fs::remove_dir_all(&dir);
    }
}
