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
    ///
    /// Meaningless for an **archive door** (task #104/#105), whose frame is a 1×1
    /// transparent sentinel — the readouts show [`size`](PhotoMeta::size) instead and
    /// must never print these. See `AppCore::info_line_parts`.
    pub w: u32,
    pub h: u32,
    /// The file's byte size, for items that display one instead of dimensions — an
    /// archive door. `None` for a photo, whose size no readout shows.
    ///
    /// Comes from `ItemSource::size_hint`, which `FsSource` resolves on the scan
    /// worker: the info line is built on the frame path and may never touch the disk.
    pub size: Option<u64>,
    /// The decoder/codec that produced it (e.g. `"JPEG"`).
    pub codec: &'static str,
    /// If this photo is an animated container (GIF/APNG/WebP, or an AVIF/HEIC
    /// sequence on macOS), which kind — so the viewer can offer on-demand playback
    /// (the ▶ P hint, task #37). `None` for a still. Sniffed during decode.
    pub animated: Option<AnimationKind>,
}

/// A byte count as a person reads it: `271 MB`, `4.2 GB`, `812 KB`.
///
/// For the **info line**, where an archive door shows its size in place of the
/// dimensions it hasn't got. Deliberately not `archive::human_gb`, which is GB-only
/// because it formats the RAM budget — a door can be a few KB or a few GB.
///
/// Decimal units (MB = 10⁶), matching what a file manager reports, so the number agrees
/// with Explorer/Finder rather than with the Details panel's exact byte count.
pub fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1_000.0;
    const MB: f64 = 1_000_000.0;
    const GB: f64 = 1_000_000_000.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.0} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_reads_like_a_file_manager() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1_000), "1 KB");
        assert_eq!(human_bytes(812_000), "812 KB");
        assert_eq!(human_bytes(271_000_000), "271 MB");
        // A decimal only where it carries information: 4.2 GB, not 271.0 MB.
        assert_eq!(human_bytes(4_200_000_000), "4.2 GB");
        assert_eq!(human_bytes(1_000_000_000), "1.0 GB");
    }
}
