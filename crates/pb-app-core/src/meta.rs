//! Per-photo display metadata — the info panel's data, cached in [`AppCore`](crate::AppCore).

use pb_decode::AnimationKind;

/// One photo's info, for the corner overlay panel. Built by the shell during decode
/// (`meta_for`) and cached in `AppCore::meta_cache` / mirrored in `current`. RAM-only,
/// dropped on exit (privacy #2).
#[derive(Clone)]
pub struct PhotoMeta {
    /// Display name — the path relative to the scan root.
    pub rel: String,
    /// Pixel width / height of the decoded image.
    pub w: u32,
    pub h: u32,
    /// The decoder/codec that produced it (e.g. `"JPEG"`).
    pub codec: &'static str,
    /// If this photo is an animated container (GIF/APNG/WebP, or an AVIF/HEIC
    /// sequence on macOS), which kind — so the viewer can offer on-demand playback
    /// (the ▶ P hint, task #37). `None` for a still. Sniffed during decode.
    pub animated: Option<AnimationKind>,
}
