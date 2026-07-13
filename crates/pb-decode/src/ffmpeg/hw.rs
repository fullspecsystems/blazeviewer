//! Hardware-accelerated video **decode** setup (task #84 plan §10):
//! VideoToolbox on macOS, VAAPI on Linux. FFmpeg runs entropy decode + motion
//! comp on the GPU decode block — the expensive part for VP9/AV1/HEVC, the
//! whole reason the macOS FFmpeg fallback exists — then we `av_hwframe_transfer`
//! the decoded surface to a CPU **NV12/P010** frame and hand it to the shared
//! [`FrameConverter`](super::convert::FrameConverter).
//!
//! This is the **same GPU-decode → CPU-readback → re-upload pattern the Windows
//! MF hardware path already ships** (`mf_hw::sample_to_nv12` locks the GPU
//! sample and copies NV12 to RAM). A true zero-copy `CVPixelBuffer`/IOSurface →
//! wgpu-Metal texture import is a documented follow-on: the renderer has **no**
//! external-texture ingest today (every texture is `COPY_DST`, filled from CPU
//! bytes), so wiring it is net-new machinery that must be validated on real
//! hardware and a display — out of scope for the readback milestone. The
//! readback path still eliminates the dominant CPU cost (the decode itself),
//! which is the `#79.10`/owner "HW decode or go home" gate.
//!
//! **Graceful degradation is structural, not hoped-for.** libavcodec calls the
//! [`get_hw_format`] callback to pick the accelerated pixel format; if the
//! hwaccel *init* then fails (e.g. AV1 on an M1/M2 that has no AV1 decode
//! block), libavcodec re-invokes `get_format` **without** the hw format, the
//! callback returns the software format, and decode proceeds in software — no
//! session failure, no mid-stream rebuild. `PB_NO_HWACCEL=1` forces software
//! everywhere; Linux VAAPI is additionally opt-in (`PB_ENABLE_VAAPI=1`) until a
//! real VAAPI box validates it (owner can't test Linux hardware, plan §16).

use ffmpeg_next as ff;

/// Which platform hardware backend to attempt for a session.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HwKind {
    /// macOS — `AV_HWDEVICE_TYPE_VIDEOTOOLBOX`, decoded surface is a `CVPixelBuffer`.
    VideoToolbox,
    /// Linux — `AV_HWDEVICE_TYPE_VAAPI`, decoded surface is a VA surface.
    /// Constructed only on Linux ([`HwKind::platform`] gates it behind
    /// `PB_ENABLE_VAAPI`); the arms below keep the code cross-platform.
    #[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
    Vaapi,
}

impl HwKind {
    fn device_type(self) -> ff::ffi::AVHWDeviceType {
        match self {
            HwKind::VideoToolbox => ff::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX,
            HwKind::Vaapi => ff::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
        }
    }

    /// The backend this build targets, or `None` where the FFmpeg path does no
    /// hardware decode (Windows uses Media Foundation instead — [`crate::mf_hw`]).
    fn platform() -> Option<HwKind> {
        #[cfg(target_os = "macos")]
        {
            Some(HwKind::VideoToolbox)
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            // Opt-in until validated on real VAAPI silicon (Linux second-class,
            // owner can't test hardware); SW is always the safe default there.
            if std::env::var_os("PB_ENABLE_VAAPI").is_some() {
                Some(HwKind::Vaapi)
            } else {
                None
            }
        }
        #[cfg(not(unix))]
        {
            None
        }
    }
}

/// True for the pixel formats a hardware decode leaves in the frame — the cue
/// to `av_hwframe_transfer` before conversion. Everything else is already a CPU
/// (software) frame the converter takes directly.
pub fn is_hw_frame(fmt: ff::format::Pixel) -> bool {
    matches!(
        fmt,
        ff::format::Pixel::VIDEOTOOLBOX | ff::format::Pixel::VAAPI
    )
}

/// Codecs shipping silicon reliably hardware-decodes. **VP8 is excluded by
/// hardware reality** (no VideoToolbox support, VAAPI VP8 extinct in modern
/// drivers — plan §10 carve-out), as is everything legacy/exotic FFmpeg covers
/// (MPEG-4 ASP, etc.): those stay on the software path by necessity, acceptable
/// because such content is low-resolution by construction.
pub fn codec_is_hw_eligible(id: ff::codec::Id) -> bool {
    use ff::codec::Id::{AV1, H264, HEVC, VP9};
    matches!(id, H264 | HEVC | VP9 | AV1)
}

/// Owns the hardware device context (`AVBufferRef`) for a decode session and
/// unrefs it on drop. The opened decoder holds its own `av_buffer_ref`, so the
/// two are order-independent (refcounted) — this must merely outlive the
/// [`try_enable`] call site's early returns, which the producer guarantees by
/// storing it beside the decoder.
pub struct HwSession {
    device_ctx: *mut ff::ffi::AVBufferRef,
}

impl Drop for HwSession {
    fn drop(&mut self) {
        unsafe { ff::ffi::av_buffer_unref(&mut self.device_ctx) };
    }
}

/// libavcodec's format-selection callback: choose the platform hardware pixel
/// format when it's offered, else fall back to the codec's first (software)
/// format. Returning the software format on the *re-query* after a failed
/// hwaccel init is exactly how graceful degradation happens (see the module
/// docs) — the callback never signals an outright failure.
///
/// The wanted format is a per-build constant (one backend per OS), so no state
/// needs threading through `AVCodecContext.opaque`.
unsafe extern "C" fn get_hw_format(
    _ctx: *mut ff::ffi::AVCodecContext,
    formats: *const ff::ffi::AVPixelFormat,
) -> ff::ffi::AVPixelFormat {
    #[cfg(target_os = "macos")]
    const WANT: ff::ffi::AVPixelFormat = ff::ffi::AVPixelFormat::AV_PIX_FMT_VIDEOTOOLBOX;
    #[cfg(all(unix, not(target_os = "macos")))]
    const WANT: ff::ffi::AVPixelFormat = ff::ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
    #[cfg(not(unix))]
    const WANT: ff::ffi::AVPixelFormat = ff::ffi::AVPixelFormat::AV_PIX_FMT_NONE;

    let none = ff::ffi::AVPixelFormat::AV_PIX_FMT_NONE;
    // SAFETY: libavcodec passes a valid list terminated by AV_PIX_FMT_NONE.
    unsafe {
        let mut p = formats;
        let first = *p;
        while *p != none {
            if *p == WANT {
                return WANT;
            }
            p = p.add(1);
        }
        // Hardware format not offered (or withdrawn after an init failure):
        // the list is now software-only; the first entry is the codec's
        // primary software format — what the default callback would pick.
        first
    }
}

/// Attach a hardware decode device to an **unopened** decoder context, if the
/// platform + codec support it and the device initializes. On success the
/// context's `hw_device_ctx` + `get_format` are set and the returned
/// [`HwSession`] must be kept alive for the life of the opened decoder. `None`
/// means "decode in software" — the caller proceeds exactly as before, so every
/// failure (kill switch, ineligible codec, no platform backend, device create
/// error) is a transparent, non-fatal fall-through.
pub fn try_enable(
    ctx: &mut ff::codec::context::Context,
    codec_id: ff::codec::Id,
) -> Option<HwSession> {
    if std::env::var_os("PB_NO_HWACCEL").is_some() {
        return None;
    }
    if !codec_is_hw_eligible(codec_id) {
        return None;
    }
    let kind = HwKind::platform()?;

    let mut device_ctx: *mut ff::ffi::AVBufferRef = std::ptr::null_mut();
    // SAFETY: out-param + null device/opts is the documented "default device"
    // form; a negative rc or null out means the backend is unavailable here.
    let rc = unsafe {
        ff::ffi::av_hwdevice_ctx_create(
            &mut device_ctx,
            kind.device_type(),
            std::ptr::null(),
            std::ptr::null_mut(),
            0,
        )
    };
    if rc < 0 || device_ctx.is_null() {
        return None;
    }

    // SAFETY: the context is freshly built from parameters and not yet opened;
    // set the hwaccel format callback (which selects `kind`'s hardware pixel
    // format) and give the context its own ref to the device.
    unsafe {
        let raw = ctx.as_mut_ptr();
        (*raw).get_format = Some(get_hw_format);
        (*raw).hw_device_ctx = ff::ffi::av_buffer_ref(device_ctx);
    }
    Some(HwSession { device_ctx })
}

/// If `frame` is a hardware surface, transfer it to a CPU **NV12/P010** frame
/// (the software format chosen by leaving `dst` clean) and copy its side data
/// (color metadata, timestamps) across; otherwise `None` (it's already a CPU
/// frame the converter takes directly). A transfer error is surfaced as a
/// decode error like any other — the graceful path is never being in a broken
/// hardware state (see the module docs).
pub fn transfer_if_hw(frame: &ff::frame::Video) -> Result<Option<ff::frame::Video>, String> {
    if !is_hw_frame(frame.format()) {
        return Ok(None);
    }
    let mut sw = ff::frame::Video::empty();
    // SAFETY: `frame` is a valid hardware AVFrame with an attached frames
    // context; `sw` is clean, so libavcodec picks the software format and
    // allocates the destination buffers itself.
    let rc = unsafe { ff::ffi::av_hwframe_transfer_data(sw.as_mut_ptr(), frame.as_ptr(), 0) };
    if rc < 0 {
        return Err(format!("hw frame transfer failed: {}", ff::Error::from(rc)));
    }
    // Transfer copies pixels only — carry color/timestamps so the converter
    // reads the same metadata it would from a software decode.
    // SAFETY: both frames are valid; copy_props never touches pixel buffers.
    unsafe {
        let _ = ff::ffi::av_frame_copy_props(sw.as_mut_ptr(), frame.as_ptr());
    }
    Ok(Some(sw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vp8_and_legacy_codecs_stay_on_software() {
        assert!(
            !codec_is_hw_eligible(ff::codec::Id::VP8),
            "VP8 carve-out (§10)"
        );
        assert!(!codec_is_hw_eligible(ff::codec::Id::MPEG4));
        assert!(!codec_is_hw_eligible(ff::codec::Id::GIF));
    }

    #[test]
    fn mainstream_codecs_are_hw_eligible() {
        for id in [
            ff::codec::Id::H264,
            ff::codec::Id::HEVC,
            ff::codec::Id::VP9,
            ff::codec::Id::AV1,
        ] {
            assert!(codec_is_hw_eligible(id), "{id:?} should try hardware");
        }
    }

    #[test]
    fn software_frame_needs_no_transfer() {
        // A plain YUV420P frame is already CPU-side — no hwaccel round trip.
        assert!(!is_hw_frame(ff::format::Pixel::YUV420P));
        assert!(!is_hw_frame(ff::format::Pixel::NV12));
        assert!(is_hw_frame(ff::format::Pixel::VIDEOTOOLBOX));
        assert!(is_hw_frame(ff::format::Pixel::VAAPI));
    }

    #[test]
    fn transfer_is_a_noop_for_software_frames() {
        let frame = ff::frame::Video::empty();
        // An empty frame has format NONE — treated as software, returns None.
        assert!(transfer_if_hw(&frame).unwrap().is_none());
    }

    /// Diagnostic (ignored, env-gated): decode the first frames of a real clip
    /// with hardware attached and report whether they come back as a GPU
    /// surface. Confirms VideoToolbox/VAAPI actually **engages** on real content
    /// — the 64×64 producer fixtures are below VideoToolbox's minimum size and
    /// degrade to software (still correct, but no proof of engagement). Run:
    ///   PB_HWTEST_CLIP=/path/clip.mov cargo test -p pb-decode --features ffvideo \
    ///     ffmpeg::hw::tests::hardware_engages_on_a_real_clip -- --ignored --nocapture
    #[test]
    #[ignore = "needs a real clip via PB_HWTEST_CLIP; validates hardware engagement"]
    fn hardware_engages_on_a_real_clip() {
        let Ok(path) = std::env::var("PB_HWTEST_CLIP") else {
            eprintln!("set PB_HWTEST_CLIP to a real video path");
            return;
        };
        crate::ffmpeg::init::ff_init();
        let mut ictx = ff::format::input(&path).expect("open input");
        let stream = ictx
            .streams()
            .best(ff::media::Type::Video)
            .expect("a video stream");
        let idx = stream.index();
        let params = stream.parameters();
        let id = params.id();
        let mut ctx = ff::codec::context::Context::from_parameters(params).expect("ctx");
        let hw = try_enable(&mut ctx, id);
        eprintln!(
            "codec {id:?} — hw device attached: {}",
            if hw.is_some() { "yes" } else { "no" }
        );
        let mut decoder = ctx.decoder().video().expect("open decoder");
        let mut decoded = ff::frame::Video::empty();
        let mut got_hw = false;
        let mut frames = 0usize;
        'outer: for (s, packet) in ictx.packets() {
            if s.index() != idx || decoder.send_packet(&packet).is_err() {
                continue;
            }
            while decoder.receive_frame(&mut decoded).is_ok() {
                frames += 1;
                let fmt = decoded.format();
                got_hw |= is_hw_frame(fmt);
                eprintln!("frame {frames}: {fmt:?} (hw surface: {})", is_hw_frame(fmt));
                // Exercise the readback the producer performs.
                if let Some(sw) = transfer_if_hw(&decoded).expect("transfer") {
                    eprintln!(
                        "  transferred -> {:?} {}x{}",
                        sw.format(),
                        sw.width(),
                        sw.height()
                    );
                }
                if frames >= 3 {
                    break 'outer;
                }
            }
        }
        eprintln!("=> hardware decode engaged on real content: {got_hw}");
        assert!(frames > 0, "decoded no frames");
    }
}
