//! Process-wide FFmpeg initialization, shared by every FFmpeg entry point
//! (`ff_live`, the `ffvideo` producer/poster) so it lives in exactly one place.

use ffmpeg_next as ff;

/// Process-wide FFmpeg init (registers codecs/formats). Idempotent + cheap —
/// call at the top of every public FFmpeg entry point.
pub fn ff_init() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = ff::init();
        // Quiet libav's stderr logging — a bad frame shouldn't spew to the console.
        ff::util::log::set_level(ff::util::log::Level::Fatal);
    });
}
