//! The Windows **video playback producer** (task #79 phase 4): a Media Foundation
//! reader thread speaking the [`VideoProducerEvent`]/[`VideoProducerMsg`] protocol.
//!
//! Demand-driven: the only blocking point is `recv()` on the session's message
//! channel — one [`Credit`](VideoProducerMsg::Credit) buys one decoded frame, and a
//! [`Stop`](VideoProducerMsg::Stop) (or channel disconnect: the session was dropped)
//! gets through even when zero credits are outstanding, so backpressure can never
//! deafen control. The reader is configured identically to the poster/metadata
//! probes (`mf_poster`): advanced video processing (rotation + YUV→RGB), all streams
//! deselected but video, color from the native media type, fitted RGB32 output —
//! poster ≡ playback in geometry, rotation, and color by construction.
//!
//! PTS discipline: timestamps are the reader's real sample times, normalized so the
//! first decoded frame is 0 (nonzero container start offsets exist in the wild —
//! MPEG-TS starts at ~766 ms in the phase-0 spike). Reads only, RAM-only: the
//! no-trace guarantee holds on the playback path exactly as it does for stills.

use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use windows::Win32::Media::MediaFoundation::{
    IMFSample, IMFSourceReader, MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
};

use crate::mf_poster::{
    fit_dims, mf_open_msg, negotiate_rgb32, open_video_reader, retire_reader, stream_info,
};
use crate::mf_video::{ensure_mf, sample_to_rgba};
use crate::video::{
    SeekGeneration, VideoColorInfo, VideoFrame, VideoInput, VideoProducerEvent, VideoProducerMsg,
    VideoProducerOptions, VideoSessionId,
};
use crate::{FitBox, PixelFormat};

/// HDR tone-map peak in scene-linear scRGB (1.0 = 203-nit BT.2408 graphics white).
/// MF mastering / content-light metadata isn't read yet, so the P010 path uses the
/// 1000-nit default the FFmpeg planar path falls back to when a container carries
/// none (`resolve_hdr_peak`); reading `MF_MT_MAX_LUMINANCE_LEVEL` is a follow-up.
const HDR_DEFAULT_PEAK: f32 = 1000.0 / 203.0;

/// A forward seek no larger than this decodes forward from the **live** reader
/// instead of recreating it (task #4, the "short-forward-hop"). Two costs vanish:
/// the SMB container re-open (measured ~222 ms on a 1080p corpus film) and the
/// keyframe backtrack — a `+2 s` tap otherwise seeks to a keyframe up to a whole
/// GOP *behind* the target and re-decodes it (measured 536 ms vs 161 ms for the
/// hop). Capped so a hop never re-runs more than a GOP or so: past this a keyframe
/// seek is competitive, and held/coarse scrubbing (`Shift`+arrow = ±10 s) falls
/// through to the recreate/in-place path below. In 100 ns units.
const FORWARD_HOP_MAX_HNS: i64 = 5 * 10_000_000; // 5 s

/// Whether a seek to `abs_target` (container time base) should **hop** — decode
/// forward from the live reader at `reader_pos` — rather than recreate the reader
/// (task #4). True only for a forward move within [`FORWARD_HOP_MAX_HNS`] of a
/// *known* live position: backward, too-far, or unknown-position seeks recreate,
/// which always lands correctly. Pure so the decision is unit-tested without MF.
fn should_hop(reader_pos: Option<i64>, abs_target: i64) -> bool {
    reader_pos.is_some_and(|p| abs_target >= p && abs_target - p <= FORWARD_HOP_MAX_HNS)
}

/// `PB_VIDEO_DIAG=1` — the Windows seek path had no instrumentation (the "measure,
/// never guess" gap task #4 called out). One line per seek: hop-vs-recreate, run-up
/// frame count, and wall time — so a slow SMB seek is a measurement, not a guess.
/// Matches the FFmpeg producer's env var.
fn video_diag() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PB_VIDEO_DIAG").is_ok_and(|v| v != "0"))
}

/// Run the producer to completion on the **current thread** (the app spawns a
/// dedicated thread for it — never the event loop). Returns when the stream ends,
/// the session says stop, the session is dropped, or decoding fails; every exit
/// path retires the reader off-thread (HEVC teardown blocks ~1 s).
///
/// `input` is the container: a filesystem path, or an archive entry's in-RAM
/// bytes (`Arc`-shared, so the seek reopens below cost a refcount, not a copy).
pub fn run_video_producer(
    input: &VideoInput,
    fit: Option<FitBox>,
    session_id: VideoSessionId,
    generation: SeekGeneration,
    events: Sender<VideoProducerEvent>,
    msgs: Receiver<VideoProducerMsg>,
    options: VideoProducerOptions,
) {
    ensure_mf();
    let fail = |error: String| {
        let _ = events.send(VideoProducerEvent::Failed { session_id, error });
    };

    let reader = match unsafe { open_video_reader(input) } {
        Ok(r) => r,
        Err(e) => return fail(e.to_string()),
    };
    let info = match unsafe { stream_info(&reader) } {
        Ok(i) => i,
        Err(e) => {
            retire_reader(reader);
            return fail(e.to_string());
        }
    };
    let (disp_w, disp_h) = info.display_dims();
    let fitted = fit.map(|f| fit_dims(disp_w, disp_h, f));

    // Pick the output format from the source colorimetry and the decode-load
    // policy (79.10 + Track B):
    //   HDR (PQ/HLG) + a 10-bit-capable renderer → P010 — hardware when the clip is
    //     heavy enough for NVDEC, else software (the video processor emits P010
    //     without a D3D device). The shader applies the EOTF, so HDR survives
    //     instead of being SDR-clamped.
    //   heavy SDR → NV12 via NVDEC (unchanged).
    //   everything else (light SDR, or HDR with no 10-bit renderer / planar off) →
    //     software RGB32, byte-for-byte unchanged (MF SDR-clamps HDR here).
    // `PB_VIDEO_FORCE_HW` overrides the decode-load policy; `PB_VIDEO_NO_PLANAR`
    // (via `options.planar`) forces HDR back to the RGB32 path.
    let nc = unsafe { crate::mf_hw::native_color(&reader, info.height) };
    let is_hdr = matches!(
        nc.transfer,
        crate::VideoTransfer::Pq | crate::VideoTransfer::Hlg
    );
    let want_hw = crate::mf_hw::hw_override()
        .unwrap_or_else(|| crate::mf_hw::pixel_rate_wants_hw(info.width, info.height, info.fps));
    let use_p010 = is_hdr && options.planar && options.supports_p010;

    // NV12/P010 subsample chroma 2×2 — odd output dims fall back to software.
    let even = |w: u32, h: u32| w > 0 && h > 0 && w.is_multiple_of(2) && h.is_multiple_of(2);

    // Hardware attempts open a NEW reader (retiring the probe reader); a software
    // path reuses the probe reader below.
    let mut manager = None;
    let mut hw_opened: Option<(IMFSourceReader, OutKind, u32, u32)> = None;
    if want_hw {
        if let Some(mgr) = unsafe { crate::mf_hw::dxgi_manager() } {
            let opened = if use_p010 {
                unsafe { crate::mf_hw::open_p010_reader(input, &mgr, fitted) }
                    .ok()
                    .map(|(r, w, h)| (r, OutKind::P010, w, h))
            } else {
                unsafe { crate::mf_hw::open_nv12_reader(input, &mgr, fitted) }
                    .ok()
                    .map(|(r, w, h)| (r, OutKind::Nv12, w, h))
            };
            match opened {
                Some((r, k, w, h)) if even(w, h) => {
                    hw_opened = Some((r, k, w, h));
                    manager = Some(mgr);
                }
                Some((r, _, _, _)) => retire_reader(r), // odd/zero dims → software
                None => {}
            }
        }
    }

    let (active_reader, mut kind, w, h) = match hw_opened {
        Some((hw_reader, k, hw_w, hw_h)) => {
            retire_reader(reader); // the plain probe reader is done
            (hw_reader, k, hw_w, hw_h)
        }
        None => {
            // Software path — reuse the probe reader. Prefer P010 for an HDR source
            // (so it isn't SDR-clamped); fall back to RGB32 if P010 won't negotiate.
            let p010 = if use_p010 {
                unsafe {
                    match fitted {
                        Some(dims) => crate::mf_hw::negotiate_p010(&reader, Some(dims))
                            .or_else(|_| crate::mf_hw::negotiate_p010(&reader, None)),
                        None => crate::mf_hw::negotiate_p010(&reader, None),
                    }
                }
                .ok()
                .filter(|&(w, h)| even(w, h))
            } else {
                None
            };
            match p010 {
                Some((w, h)) => (reader, OutKind::P010, w, h),
                None => {
                    let negotiated = unsafe {
                        match fitted {
                            Some(dims) => negotiate_rgb32(&reader, Some(dims))
                                .or_else(|_| negotiate_rgb32(&reader, None)),
                            None => negotiate_rgb32(&reader, None),
                        }
                    };
                    match negotiated {
                        Ok((w, h, stride)) => (reader, OutKind::Rgb32 { stride }, w, h),
                        Err(e) => {
                            retire_reader(reader);
                            return fail(mf_open_msg(e));
                        }
                    }
                }
            }
        }
    };
    if w == 0 || h == 0 {
        retire_reader(active_reader);
        return fail("video has no frames".into());
    }
    let format = kind.format();
    let _ = events.send(VideoProducerEvent::Opened {
        session_id,
        duration: info.duration,
        width: w,
        height: h,
        has_audio: info.has_audio,
        frame_bytes: format.frame_bytes(w, h) as u64,
    });
    // The single-application contract, per output kind.
    let color = match kind {
        // P010: pixels arrive raw (PQ/HLG-encoded BT.2020 10-bit YUV) — the renderer
        // applies matrix + range + EOTF + primaries in-shader exactly once (mirrors
        // the FFmpeg planar path's `video_color_info_planar`).
        OutKind::P010 => VideoColorInfo {
            transform: crate::ColorTransform::from_cicp(nc.primaries, 8, 0, true),
            cicp: None,
            full_range: nc.full_range,
            yuv_matrix: nc.yuv_matrix,
            transfer: nc.transfer,
            peak: HDR_DEFAULT_PEAK,
        },
        // NV12: raw YUV, the renderer applies matrix + range once (SDR).
        OutKind::Nv12 => VideoColorInfo {
            transform: info.color,
            cicp: None,
            full_range: nc.full_range,
            yuv_matrix: nc.yuv_matrix,
            transfer: crate::VideoTransfer::SrgbLike,
            peak: 1.0,
        },
        // RGB32: MF already applied matrix + range (fields inert); SDR-clamped.
        OutKind::Rgb32 { .. } => VideoColorInfo {
            transform: info.color,
            cicp: None,
            full_range: true,
            yuv_matrix: nc.yuv_matrix,
            transfer: crate::VideoTransfer::SrgbLike,
            peak: 1.0,
        },
    };

    // The credit/command/seek loop. Blocking recv IS the select: a Stop or a
    // SeekTo (or the session dropping its sender) wakes us regardless of credit
    // starvation. A SeekTo zeroes the credit balance — only credits received
    // after it (which the session sends after flushing) count.
    let mut origin: Option<i64> = None;
    let mut gen = generation;
    let mut credits: usize = 0;
    let mut pending: Option<(Duration, crate::video::SeekGeneration)> = None;
    let mut active: Option<IMFSourceReader> = Some(active_reader);
    // Where the live reader is positioned — the abs ts (container time base) of the
    // last frame read from it. Drives the short-forward-hop (task #4): a forward seek
    // within reach of this decodes forward rather than recreating the reader. `None`
    // whenever there is no live reader (start before the first frame; parked at EOS).
    let mut reader_pos: Option<i64> = None;

    'outer: loop {
        // 1. Absorb messages; block only when there is nothing to do.
        loop {
            let msg = if credits == 0 && pending.is_none() {
                match msgs.recv() {
                    Ok(m) => m,
                    Err(_) => break 'outer,
                }
            } else {
                match msgs.try_recv() {
                    Ok(m) => m,
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(_) => break 'outer,
                }
            };
            match msg {
                VideoProducerMsg::Stop => break 'outer,
                VideoProducerMsg::Credit => credits += 1,
                VideoProducerMsg::SeekTo { target, generation } => {
                    pending = Some((target, generation));
                    credits = 0;
                }
            }
        }

        // 2. Land a pending seek: retire the old reader (repositioning a warm
        // HEVC reader blocks ~1 s; a fresh one positions before its first read in
        // ~0 ms — spike E), open at the target, decode forward to the first frame
        // ≥ it. A newer SeekTo supersedes every stage; a superseded landing never
        // publishes a frame.
        if let Some((target, g)) = pending.take() {
            gen = g;
            let abs_target = origin
                .unwrap_or(0)
                .saturating_add((target.as_nanos() / 100) as i64);
            // Short-forward-hop (task #4): a small FORWARD seek from a live reader
            // decodes forward in place — the decode-forward loop below reads from the
            // reader wherever it sits, so a reader already at `reader_pos` just before
            // the target needs no reopen and no keyframe backtrack. Anything else
            // (backward, large forward, no live reader, or parked at EOS) recreates.
            let hop = active.is_some() && should_hop(reader_pos, abs_target);
            let seek_started = video_diag().then(std::time::Instant::now);
            let mut run_up_frames = 0u32;
            if !hop {
                if let Some(r) = active.take() {
                    retire_reader(r);
                }
                let reader =
                    match unsafe { reopen_at(input, (w, h), abs_target, manager.as_ref(), kind) } {
                        Ok((r, k)) => {
                            kind = k;
                            r
                        }
                        Err(e) => {
                            fail(e);
                            break 'outer;
                        }
                    };
                active = Some(reader);
                // The fresh reader sits on a keyframe ≤ abs_target; its exact ts isn't
                // known until the first read, so a subsequent hop must not trust a
                // stale position. The run-up's first frame re-establishes it.
                reader_pos = None;
            }
            let mut landed: Option<(i64, Vec<u8>)> = None;
            loop {
                // Watch for supersede/stop between reads (latest-value).
                match msgs.try_recv() {
                    Ok(VideoProducerMsg::Stop) => break 'outer,
                    Ok(VideoProducerMsg::Credit) => credits += 1,
                    Ok(VideoProducerMsg::SeekTo { target, generation }) => {
                        pending = Some((target, generation));
                        credits = 0;
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    Err(_) => break 'outer,
                }
                let reader = active.as_ref().expect("reader set above");
                match unsafe { read_raw(reader, w, h, &mut kind) } {
                    Ok(Read1Raw::Eos) => {
                        // Sought at/near the end: the stream is over under the
                        // new generation; the reader is spent.
                        let _ = events.send(VideoProducerEvent::EndOfStream {
                            session_id,
                            seek_generation: gen,
                        });
                        if let Some(r) = active.take() {
                            retire_reader(r);
                        }
                        reader_pos = None;
                        break;
                    }
                    Ok(Read1Raw::Gap) => {}
                    Ok(Read1Raw::Frame { ts, sample }) => {
                        // Every read advances the reader — track it even for discarded
                        // run-up frames, so a supersede mid-run-up leaves `reader_pos`
                        // truthful for the next hop decision.
                        reader_pos = Some(ts);
                        if ts >= abs_target {
                            // The landing frame is the ONLY one read back/converted;
                            // the run-up frames above dropped their samples unconverted
                            // (task #4 — the recreate-seek stall was 30× this copy).
                            let pixels = match unsafe { convert_sample(&sample, w, h, kind) } {
                                Ok(p) => p,
                                Err(e) => {
                                    fail(e);
                                    break 'outer;
                                }
                            };
                            landed = Some((ts, pixels));
                            break;
                        }
                        // Keyframe→target run-up: drop the raw sample (no readback, no
                        // swizzle), keep decoding forward.
                        run_up_frames += 1;
                    }
                    Err(e) => {
                        fail(e);
                        break 'outer;
                    }
                }
            }
            if let Some((ts, pixels)) = landed {
                if let Some(t0) = seek_started {
                    eprintln!(
                        "[pb-video] seek {} to {:.1}s: {} run-up frames in {:?}",
                        if hop { "HOP" } else { "recreate" },
                        (abs_target - origin.unwrap_or(0)).max(0) as f64 / 1e7,
                        run_up_frames,
                        t0.elapsed(),
                    );
                }
                // The landing frame consumes a credit like any other (the session
                // granted fresh ones right behind the SeekTo). Block for one if
                // needed — Stop/SeekTo still interrupt.
                while credits == 0 && pending.is_none() {
                    match msgs.recv() {
                        Ok(VideoProducerMsg::Credit) => credits += 1,
                        Ok(VideoProducerMsg::SeekTo { target, generation }) => {
                            pending = Some((target, generation));
                            credits = 0;
                        }
                        Ok(VideoProducerMsg::Stop) | Err(_) => break 'outer,
                    }
                }
                if pending.is_some() {
                    continue 'outer; // superseded before publish — never flash it
                }
                origin.get_or_insert(abs_target - (target.as_nanos() / 100) as i64);
                let pts_hns = (ts - origin.unwrap_or(0)).max(0) as u64;
                let frame = VideoFrame {
                    session_id,
                    seek_generation: gen,
                    pts: Duration::from_nanos(pts_hns * 100),
                    width: w,
                    height: h,
                    format,
                    pixels,
                    color: color.clone(),
                };
                if events.send(VideoProducerEvent::Frame(frame)).is_err() {
                    break 'outer;
                }
                credits -= 1;
            }
            continue 'outer;
        }

        // 3. Spend one credit on the next sequential frame.
        if credits > 0 {
            let Some(reader) = active.as_ref() else {
                // No reader (parked after EOS): these credits are stale — a seek
                // recreates the reader and resets the balance.
                credits = 0;
                continue;
            };
            match unsafe { read_one(reader, w, h, &mut kind) } {
                Ok(Read1::Eos) => {
                    let _ = events.send(VideoProducerEvent::EndOfStream {
                        session_id,
                        seek_generation: gen,
                    });
                    // Park (don't exit): a later SeekTo replays/rewinds by
                    // recreating the reader; Stop/disconnect ends the thread.
                    if let Some(r) = active.take() {
                        retire_reader(r);
                    }
                    reader_pos = None;
                }
                Ok(Read1::Gap) => {}
                Ok(Read1::Frame { ts, pixels }) => {
                    reader_pos = Some(ts); // the live reader advanced (hop reference)
                    let o = *origin.get_or_insert(ts);
                    let pts_hns = (ts - o).max(0) as u64;
                    let frame = VideoFrame {
                        session_id,
                        seek_generation: gen,
                        pts: Duration::from_nanos(pts_hns * 100),
                        width: w,
                        height: h,
                        format,
                        pixels,
                        color: color.clone(),
                    };
                    if events.send(VideoProducerEvent::Frame(frame)).is_err() {
                        break 'outer;
                    }
                    credits -= 1;
                }
                Err(e) => {
                    fail(e);
                    break 'outer;
                }
            }
        }
    }
    if let Some(r) = active {
        retire_reader(r);
    }
}

/// Which decoded output this producer negotiated (task 79.10) — fixed for the
/// session; a seek recreates the reader in the same kind.
#[derive(Clone, Copy)]
enum OutKind {
    /// Software decode, RGB32 output, BGRX→RGBA swizzle (the shipping path).
    Rgb32 { stride: i32 },
    /// NV12 planes via `Lock2DSize` — SDR, hardware decode (DXGI manager).
    Nv12,
    /// P010 (10-bit) planes via `Lock2DSize` — HDR (PQ/HLG). Hardware when the clip
    /// is heavy, else software (the video processor emits P010 with no D3D device).
    P010,
}

impl OutKind {
    fn format(self) -> PixelFormat {
        match self {
            OutKind::Rgb32 { .. } => PixelFormat::Rgba8,
            OutKind::Nv12 => PixelFormat::Nv12,
            OutKind::P010 => PixelFormat::P010,
        }
    }
}

/// One `ReadSample` step, handling gap ticks, mid-stream stride requery, and the
/// size-change failure. `Frame`'s `ts` is the reader's raw (container) timestamp;
/// `pixels` is packed per `OutKind` (RGBA8, or Y+UV planes).
enum Read1 {
    Frame { ts: i64, pixels: Vec<u8> },
    Eos,
    Gap,
}

/// The **un-converted** result of one `ReadSample` — the raw MF sample plus its
/// timestamp, with the readback/swizzle deliberately *not* done yet. The seek
/// run-up decodes forward through these and drops the discarded ones without ever
/// touching their pixels: on the software path that skips the ~12 ms/frame BGRX→
/// RGBA swizzle, on the hw path the ~5 ms/frame `Lock2DSize` readback — the whole
/// recreate-seek stall (measured 347 ms → the landing frame alone at 4K60; the
/// FFmpeg producer's convert-skip, mirrored here for task #4). Only the landing
/// frame is converted via [`convert_sample`].
enum Read1Raw {
    Frame { ts: i64, sample: IMFSample },
    Eos,
    Gap,
}

/// `ReadSample` + the gap/EOS/size-change handling, returning the raw sample
/// unconverted (see [`Read1Raw`]). Updates `kind`'s stride on a mid-stream
/// media-type change so a later [`convert_sample`] uses the right pitch.
unsafe fn read_raw(
    reader: &windows::Win32::Media::MediaFoundation::IMFSourceReader,
    w: u32,
    h: u32,
    kind: &mut OutKind,
) -> Result<Read1Raw, String> {
    let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let mut flags = 0u32;
    let mut ts = 0i64;
    let mut sample: Option<IMFSample> = None;
    reader
        .ReadSample(
            video,
            0,
            None,
            Some(&mut flags),
            Some(&mut ts),
            Some(&mut sample),
        )
        .map_err(mf_open_msg)?;
    if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
        return Ok(Read1Raw::Eos);
    }
    // A mid-stream media-type change can move the stride (the size is fixed by
    // our negotiated output type); re-query it. NV12 reads its pitch per sample
    // (`Lock2DSize`), so only the size check applies there.
    if flags != 0 {
        if let Ok((nw, nh, ns)) = negotiate_current(reader) {
            if (nw, nh) != (w, h) {
                return Err("video changed size mid-stream".into());
            }
            if let OutKind::Rgb32 { stride } = kind {
                *stride = ns;
            }
        }
    }
    let Some(sample) = sample else {
        return Ok(Read1Raw::Gap);
    };
    Ok(Read1Raw::Frame { ts, sample })
}

/// Pack one raw sample into `pixels` per `OutKind` (RGBA8 swizzle, or the NV12
/// `Lock2DSize` plane readback) — the per-frame cost the run-up avoids for every
/// frame it discards.
unsafe fn convert_sample(
    sample: &IMFSample,
    w: u32,
    h: u32,
    kind: OutKind,
) -> Result<Vec<u8>, String> {
    match kind {
        OutKind::Rgb32 { stride } => {
            sample_to_rgba(sample, w, h, stride).map_err(|e| format!("Media Foundation: {e}"))
        }
        OutKind::Nv12 => {
            crate::mf_hw::sample_to_nv12(sample, w, h).map_err(|e| format!("Media Foundation: {e}"))
        }
        OutKind::P010 => {
            crate::mf_hw::sample_to_p010(sample, w, h).map_err(|e| format!("Media Foundation: {e}"))
        }
    }
}

unsafe fn read_one(
    reader: &windows::Win32::Media::MediaFoundation::IMFSourceReader,
    w: u32,
    h: u32,
    kind: &mut OutKind,
) -> Result<Read1, String> {
    match read_raw(reader, w, h, kind)? {
        Read1Raw::Eos => Ok(Read1::Eos),
        Read1Raw::Gap => Ok(Read1::Gap),
        Read1Raw::Frame { ts, sample } => {
            let pixels = convert_sample(&sample, w, h, *kind)?;
            Ok(Read1::Frame { ts, pixels })
        }
    }
}

/// Fresh reader for a seek landing: open + negotiate the SAME output kind and
/// geometry the session fixed at start, then position **before the first read**
/// (~0 ms; spike E). `kind` picks the format (a P010 seek reopens P010, etc.);
/// `manager` (present only for a hardware session) picks hardware vs software for
/// the planar formats. An in-RAM input reopens over the same shared bytes (a fresh
/// stream instance — no re-read, no copy).
unsafe fn reopen_at(
    input: &VideoInput,
    dims: (u32, u32),
    position_hns: i64,
    manager: Option<&windows::Win32::Media::MediaFoundation::IMFDXGIDeviceManager>,
    kind: OutKind,
) -> Result<
    (
        windows::Win32::Media::MediaFoundation::IMFSourceReader,
        OutKind,
    ),
    String,
> {
    let size_changed = || "video output size changed across a seek".to_string();
    let (reader, new_kind) = match kind {
        OutKind::Nv12 => {
            let mgr = manager.ok_or_else(|| "NV12 seek without a device manager".to_string())?;
            let (reader, nw, nh) =
                crate::mf_hw::open_nv12_reader(input, mgr, Some(dims)).map_err(mf_open_msg)?;
            if (nw, nh) != dims {
                retire_reader(reader);
                return Err(size_changed());
            }
            (reader, OutKind::Nv12)
        }
        OutKind::P010 => {
            // Hardware P010 reuses the manager; a software P010 session (a light HDR
            // clip) reopens a plain reader and negotiates P010 on it.
            let (reader, nw, nh) = match manager {
                Some(mgr) => {
                    crate::mf_hw::open_p010_reader(input, mgr, Some(dims)).map_err(mf_open_msg)?
                }
                None => {
                    let reader = open_video_reader(input).map_err(|e| e.to_string())?;
                    let (nw, nh) = crate::mf_hw::negotiate_p010(&reader, Some(dims))
                        .or_else(|_| crate::mf_hw::negotiate_p010(&reader, None))
                        .map_err(mf_open_msg)?;
                    (reader, nw, nh)
                }
            };
            if (nw, nh) != dims {
                retire_reader(reader);
                return Err(size_changed());
            }
            (reader, OutKind::P010)
        }
        OutKind::Rgb32 { .. } => {
            let reader = open_video_reader(input).map_err(|e| e.to_string())?;
            let (nw, nh, stride) = negotiate_rgb32(&reader, Some(dims))
                .or_else(|_| negotiate_rgb32(&reader, None))
                .map_err(mf_open_msg)?;
            if (nw, nh) != dims {
                retire_reader(reader);
                return Err(size_changed());
            }
            (reader, OutKind::Rgb32 { stride })
        }
    };
    let pos = crate::mf_poster::propvariant_i8(position_hns.max(0));
    if let Err(e) = reader.SetCurrentPosition(&windows::core::GUID::zeroed(), &pos) {
        retire_reader(reader);
        return Err(mf_open_msg(e));
    }
    Ok((reader, new_kind))
}

/// Re-read the negotiated output geometry/stride after a media-type-change tick.
unsafe fn negotiate_current(
    reader: &windows::Win32::Media::MediaFoundation::IMFSourceReader,
) -> windows::core::Result<(u32, u32, i32)> {
    use windows::Win32::Media::MediaFoundation::{MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE};
    let cur = reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)?;
    let packed = cur.GetUINT64(&MF_MT_FRAME_SIZE)?;
    let (w, h) = ((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32);
    let stride = cur
        .GetUINT32(&MF_MT_DEFAULT_STRIDE)
        .map(|s| s as i32)
        .unwrap_or((w * 4) as i32);
    Ok((w, h, stride))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use std::time::Instant;

    const SID: VideoSessionId = VideoSessionId(42);
    const GEN: SeekGeneration = SeekGeneration::FIRST;

    fn fixture() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/video/black_then_color.mp4")
    }

    fn spawn(path: std::path::PathBuf) -> (Sender<VideoProducerMsg>, Receiver<VideoProducerEvent>) {
        spawn_input(VideoInput::Path(path))
    }

    fn spawn_input(input: VideoInput) -> (Sender<VideoProducerMsg>, Receiver<VideoProducerEvent>) {
        let (events_tx, events_rx) = channel();
        let (msgs_tx, msgs_rx) = channel();
        std::thread::spawn(move || {
            run_video_producer(
                &input,
                None,
                SID,
                GEN,
                events_tx,
                msgs_rx,
                VideoProducerOptions::default(),
            );
        });
        (msgs_tx, events_rx)
    }

    /// Diagnostic (opt-in): where does a Windows seek's wall-clock go **over SMB**?
    /// The spike measured the reader open at ~4-20 ms *locally*; this measures it plus
    /// the decode-forward run-up against a real (network) file, and A/B's the current
    /// recreate-the-reader strategy vs. an in-place `SetCurrentPosition` and a
    /// decode-forward hop — the two candidate optimizations (task #4).
    /// `PB_SEEK_CLIP=\\server\share\film.mkv cargo test -p pb-decode --release seek_cost_probe -- --ignored --nocapture`
    #[test]
    #[ignore = "diagnostic — needs PB_SEEK_CLIP (ideally over SMB)"]
    fn seek_cost_probe() {
        use std::time::Instant;
        let Ok(clip) = std::env::var("PB_SEEK_CLIP") else {
            eprintln!("PB_SEEK_CLIP not set — skipping");
            return;
        };
        let input = VideoInput::Path(clip.clone().into());
        ensure_mf();
        unsafe {
            // Establish geometry, duration, codec, and the origin (first frame ts) —
            // the software RGB32 path (H.264 corpus decodes fine in software; the
            // hardware NV12 path needs a DXGI manager the producer owns).
            let t = Instant::now();
            let reader0 = open_video_reader(&input).expect("open");
            let info = stream_info(&reader0).expect("stream info");
            let (w, h, _stride) = negotiate_rgb32(&reader0, None).expect("negotiate");
            let mut kind = OutKind::Rgb32 {
                stride: (w * 4) as i32,
            };
            eprintln!(
                "clip: {} {}x{} {:?} dur={:?} | cold open+negotiate {:?}",
                info.codec,
                w,
                h,
                info.color,
                info.duration,
                t.elapsed()
            );
            // First frame → origin (container time base).
            let origin = loop {
                match read_one(&reader0, w, h, &mut kind).expect("read") {
                    Read1::Frame { ts, .. } => break ts,
                    Read1::Gap => {}
                    Read1::Eos => panic!("no frames"),
                }
            };
            let dur_hns = info.duration.map_or(0, |d| (d.as_nanos() / 100) as i64);

            // Decode-forward from a reader until a frame's ts >= `abs_target`;
            // returns (frames_discarded, wall, landed_ts).
            let run_up = |reader: &IMFSourceReader, kind: &mut OutKind, abs_target: i64| {
                let t = Instant::now();
                let mut n = 0u32;
                loop {
                    match read_one(reader, w, h, kind).expect("read") {
                        Read1::Frame { ts, .. } => {
                            if ts >= abs_target {
                                break (n, t.elapsed(), ts);
                            }
                            n += 1;
                        }
                        Read1::Gap => {}
                        Read1::Eos => break (n, t.elapsed(), -1),
                    }
                }
            };

            // ── A. The CURRENT strategy: recreate the reader at ~40%, then run up.
            let mid = origin + dur_hns * 2 / 5;
            let t = Instant::now();
            let (reader_a, mut kind_a) =
                reopen_at(&input, (w, h), mid, None, OutKind::Rgb32 { stride: 0 })
                    .expect("reopen_at");
            let reopen_wall = t.elapsed();
            let (fa, run_a, land_a) = run_up(&reader_a, &mut kind_a, mid);
            eprintln!(
                "\n[A] RECREATE seek to 40%: reopen(open+negotiate+setpos) {:?} + run-up {} frames {:?} = TOTAL {:?}  (landed {:.1}s past target)",
                reopen_wall,
                fa,
                run_a,
                reopen_wall + run_a,
                (land_a - mid) as f64 / 1e7,
            );

            // ── B. IN-PLACE on the now-WARM reader to ~60% (SetCurrentPosition +
            //       run-up). For H.264 the warm reposition is cheap (spike); for HEVC
            //       it's the ~1 s MFT penalty. This is the number that decides whether
            //       in-place beats recreate for THIS clip's codec over SMB.
            let mid2 = origin + dur_hns * 3 / 5;
            let t = Instant::now();
            let pv = crate::mf_poster::propvariant_i8(mid2.max(0));
            reader_a
                .SetCurrentPosition(&windows::core::GUID::zeroed(), &pv)
                .expect("in-place seek");
            let setpos_wall = t.elapsed();
            let (fb, run_b, land_b) = run_up(&reader_a, &mut kind_a, mid2);
            eprintln!(
                "[B] IN-PLACE seek to 60% (warm reader): SetCurrentPosition {:?} + run-up {} frames {:?} = TOTAL {:?}  (landed {:.1}s past target)",
                setpos_wall,
                fb,
                run_b,
                setpos_wall + run_b,
                (land_b - mid2) as f64 / 1e7,
            );

            // ── C. A small +2 s FORWARD tap, two ways from the same warm position.
            //       C1 = in-place seek +2 s; C2 = decode-forward-only (no seek at all,
            //       the "short-forward-hop" — mirrors the FFmpeg backend).
            let here = land_b; // where B left the reader
            let plus2 = here + 2 * 10_000_000;
            let t = Instant::now();
            let pv = crate::mf_poster::propvariant_i8(plus2.max(0));
            reader_a
                .SetCurrentPosition(&windows::core::GUID::zeroed(), &pv)
                .expect("seek +2s");
            let setpos2 = t.elapsed();
            let (fc1, run_c1, _) = run_up(&reader_a, &mut kind_a, plus2);
            eprintln!(
                "\n[C1] +2s tap via IN-PLACE seek: SetCurrentPosition {:?} + run-up {} frames {:?} = TOTAL {:?}",
                setpos2,
                fc1,
                run_c1,
                setpos2 + run_c1,
            );
            // C2: a fresh warm reader positioned at `here`, then decode-forward 2 s
            // WITHOUT seeking (what a short-forward-hop would do from the live reader).
            let (reader_c, mut kind_c) =
                reopen_at(&input, (w, h), here, None, OutKind::Rgb32 { stride: 0 })
                    .expect("reopen at here");
            let _ = run_up(&reader_c, &mut kind_c, here); // land at `here`
            let (fc2, run_c2, _) = run_up(&reader_c, &mut kind_c, here + 2 * 10_000_000);
            eprintln!(
                "[C2] +2s tap via DECODE-FORWARD only (no seek): {} frames {:?}  <-- the short-forward-hop",
                fc2, run_c2,
            );
            retire_reader(reader0);
            retire_reader(reader_a);
            retire_reader(reader_c);
            eprintln!("\nreopen_wall is the SMB re-open cost the local spike never saw; compare [A] vs [B] for recreate-vs-in-place, and [C1] vs [C2] for the forward-hop.");
        }
    }

    /// The short-forward-hop decision (task #4). The hop must fire ONLY for a forward
    /// move within the cap from a known live position — everything else recreates,
    /// which always lands correctly. The stale/backward cases are the ones that would
    /// silently break (decode-forward can never reach a target behind the reader).
    #[test]
    fn hop_only_on_a_small_forward_move_from_a_known_position() {
        let s = 10_000_000i64; // 1 s in hns
        assert!(should_hop(Some(10 * s), 12 * s), "a +2 s tap hops");
        assert!(
            should_hop(Some(10 * s), 10 * s),
            "seeking where we sit hops"
        );
        assert!(
            should_hop(Some(10 * s), 10 * s + FORWARD_HOP_MAX_HNS),
            "exactly at the cap still hops"
        );
        assert!(
            !should_hop(Some(10 * s), 10 * s + FORWARD_HOP_MAX_HNS + 1),
            "one tick past the cap recreates"
        );
        assert!(
            !should_hop(Some(10 * s), 8 * s),
            "a backward seek recreates"
        );
        assert!(
            !should_hop(None, 12 * s),
            "unknown position (fresh/parked reader) recreates"
        );
    }

    /// Diagnostic (opt-in): drive the REAL producer over SMB and watch the hop fire.
    /// Unlike `seek_cost_probe` (raw MF ops), this exercises the producer's seek
    /// path — the code the app runs — so `PB_VIDEO_DIAG` prints HOP vs recreate.
    /// `PB_SEEK_CLIP=\\server\share\film.mkv PB_VIDEO_DIAG=1 cargo test -p pb-decode --release producer_seek_diag -- --ignored --nocapture`
    #[test]
    #[ignore = "diagnostic — needs PB_SEEK_CLIP + PB_VIDEO_DIAG=1"]
    fn producer_seek_diag() {
        let Ok(clip) = std::env::var("PB_SEEK_CLIP") else {
            eprintln!("PB_SEEK_CLIP not set — skipping");
            return;
        };
        let (msgs, events) = spawn_input(VideoInput::Path(clip.into()));
        let _ = events
            .recv_timeout(Duration::from_secs(20))
            .expect("opened");
        // Land somewhere mid-film (recreate), then probe: +2 s (hop), +10 s (recreate,
        // past the cap), backward (recreate). Each diag line names which path ran.
        let seek = |ms: u64, g: u64| {
            msgs.send(VideoProducerMsg::SeekTo {
                target: Duration::from_millis(ms),
                generation: SeekGeneration(g),
            })
            .unwrap();
            msgs.send(VideoProducerMsg::Credit).unwrap();
            match events
                .recv_timeout(Duration::from_secs(30))
                .expect("landing")
            {
                VideoProducerEvent::Frame(f) => eprintln!("  landed at {:?}", f.pts),
                other => panic!("expected a landing, got {other:?}"),
            }
        };
        eprintln!("-- initial seek to 30 s (recreate expected):");
        seek(30_000, 1);
        eprintln!("-- +2 s tap to 32 s (HOP expected):");
        seek(32_000, 2);
        eprintln!("-- +2 s tap to 34 s (HOP expected):");
        seek(34_000, 3);
        eprintln!("-- +10 s Shift-tap to 44 s (recreate expected — past the 5 s cap):");
        seek(44_000, 4);
        eprintln!("-- backward to 20 s (recreate expected):");
        seek(20_000, 5);
        let _ = msgs.send(VideoProducerMsg::Stop);
    }

    #[test]
    fn producer_streams_the_fixture_on_credits_and_reports_eos() {
        let (msgs, events) = spawn(fixture());
        // Opened arrives unprompted with the stream facts.
        let opened = events
            .recv_timeout(Duration::from_secs(10))
            .expect("opened");
        match opened {
            VideoProducerEvent::Opened {
                session_id,
                duration,
                width,
                height,
                has_audio,
                frame_bytes,
            } => {
                assert_eq!(session_id, SID);
                assert_eq!((width, height), (64, 64));
                assert!(duration.expect("mp4 duration") > Duration::from_millis(800));
                assert!(!has_audio, "the black/color fixture is silent");
                assert_eq!(frame_bytes, 64 * 64 * 4, "credit size = negotiated output");
            }
            other => panic!("expected Opened, got {other:?}"),
        }

        // Credits pull frames one at a time, PTS normalized + monotonic.
        let mut last_pts = None;
        let mut frames = 0usize;
        loop {
            msgs.send(VideoProducerMsg::Credit).unwrap();
            match events.recv_timeout(Duration::from_secs(10)).expect("event") {
                VideoProducerEvent::Frame(f) => {
                    assert_eq!(f.session_id, SID);
                    assert!(f.is_well_formed());
                    if frames == 0 {
                        assert_eq!(f.pts, Duration::ZERO, "first PTS normalized to 0");
                    }
                    if let Some(prev) = last_pts {
                        assert!(f.pts > prev, "PTS must be monotonic");
                    }
                    last_pts = Some(f.pts);
                    frames += 1;
                }
                VideoProducerEvent::EndOfStream {
                    session_id,
                    seek_generation,
                } => {
                    assert_eq!(session_id, SID);
                    assert_eq!(seek_generation, GEN);
                    break;
                }
                other => panic!("unexpected event {other:?}"),
            }
        }
        // ~30 frames (1 s @ 30 fps).
        assert!(
            (25..=35).contains(&frames),
            "expected ~30 fixture frames, got {frames}"
        );
    }

    /// Phase 6: a SeekTo recreates the reader at the target and the next published
    /// frame carries the new generation with pts ≥ the target; a superseding
    /// SeekTo sent before the credit means the older landing never publishes.
    #[test]
    fn seek_lands_at_the_target_with_the_new_generation() {
        let (msgs, events) = spawn(fixture());
        let _ = events
            .recv_timeout(Duration::from_secs(10))
            .expect("opened");
        // Pull one normal frame first (establishes the PTS origin).
        msgs.send(VideoProducerMsg::Credit).unwrap();
        match events.recv_timeout(Duration::from_secs(10)).expect("frame") {
            VideoProducerEvent::Frame(f) => assert_eq!(f.pts, Duration::ZERO),
            other => panic!("expected a frame, got {other:?}"),
        }

        // Seek to 0.5 s (fixture is ~1 s @ 30 fps).
        let g1 = SeekGeneration(1);
        msgs.send(VideoProducerMsg::SeekTo {
            target: Duration::from_millis(500),
            generation: g1,
        })
        .unwrap();
        msgs.send(VideoProducerMsg::Credit).unwrap();
        match events
            .recv_timeout(Duration::from_secs(10))
            .expect("landing")
        {
            VideoProducerEvent::Frame(f) => {
                assert_eq!(f.seek_generation, g1, "landing carries the new generation");
                assert!(
                    f.pts >= Duration::from_millis(500) && f.pts < Duration::from_millis(700),
                    "landed at {:?}",
                    f.pts
                );
            }
            other => panic!("expected the landing frame, got {other:?}"),
        }

        // Supersede: two seeks, credits only after — the first must never flash.
        let g2 = SeekGeneration(2);
        let g3 = SeekGeneration(3);
        msgs.send(VideoProducerMsg::SeekTo {
            target: Duration::from_millis(100),
            generation: g2,
        })
        .unwrap();
        msgs.send(VideoProducerMsg::SeekTo {
            target: Duration::from_millis(700),
            generation: g3,
        })
        .unwrap();
        msgs.send(VideoProducerMsg::Credit).unwrap();
        match events
            .recv_timeout(Duration::from_secs(10))
            .expect("landing")
        {
            VideoProducerEvent::Frame(f) => {
                assert_eq!(f.seek_generation, g3, "only the newest seek publishes");
                assert!(f.pts >= Duration::from_millis(700), "landed at {:?}", f.pts);
            }
            other => panic!("expected the superseding landing, got {other:?}"),
        }
    }

    /// Phase 6: after EOS the producer parks (doesn't exit) so a replay/rewind
    /// SeekTo still works on the same session.
    #[test]
    fn seek_after_eos_replays_from_the_target() {
        let (msgs, events) = spawn(fixture());
        let _ = events
            .recv_timeout(Duration::from_secs(10))
            .expect("opened");
        // Drain the whole stream.
        loop {
            msgs.send(VideoProducerMsg::Credit).unwrap();
            match events.recv_timeout(Duration::from_secs(10)).expect("event") {
                VideoProducerEvent::Frame(_) => {}
                VideoProducerEvent::EndOfStream { .. } => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        // Replay: seek to 0 on the parked producer.
        let g1 = SeekGeneration(1);
        msgs.send(VideoProducerMsg::SeekTo {
            target: Duration::ZERO,
            generation: g1,
        })
        .unwrap();
        msgs.send(VideoProducerMsg::Credit).unwrap();
        match events
            .recv_timeout(Duration::from_secs(10))
            .expect("replay")
        {
            VideoProducerEvent::Frame(f) => {
                assert_eq!(f.seek_generation, g1);
                assert_eq!(f.pts, Duration::ZERO, "replay starts at zero");
            }
            other => panic!("expected the replay frame, got {other:?}"),
        }
    }

    #[test]
    fn stop_interrupts_a_credit_starved_producer_quickly() {
        let (msgs, events) = spawn(fixture());
        let _ = events
            .recv_timeout(Duration::from_secs(10))
            .expect("opened");
        // No credits outstanding: the producer is blocked on recv. Stop must
        // reach it and the channel must close promptly.
        let t0 = Instant::now();
        msgs.send(VideoProducerMsg::Stop).unwrap();
        // Drain any in-flight events; a receive error = disconnect = producer exited.
        while events.recv_timeout(Duration::from_secs(10)).is_ok() {}
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "stop must not hang behind backpressure"
        );
    }

    /// Phase 5: `Opened` reports the audio track's presence — the signal that
    /// starts the shell audio player (silent clips never get one).
    #[test]
    fn opened_reports_an_audio_track_when_one_exists() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/video/color_with_tone.mp4");
        let (_msgs, events) = spawn(path);
        match events
            .recv_timeout(Duration::from_secs(10))
            .expect("opened")
        {
            VideoProducerEvent::Opened { has_audio, .. } => {
                assert!(has_audio, "the tone fixture has an AAC track");
            }
            other => panic!("expected Opened, got {other:?}"),
        }
    }

    /// Task 79.10: the hardware reader plumbing decodes the committed fixture to
    /// well-formed NV12 planes (even dims, packed Y+UV). Skips gracefully where
    /// no D3D11 hardware device exists (CI VM without a GPU) — the producer's
    /// fallback covers that case in production.
    #[test]
    fn hw_reader_decodes_the_fixture_to_nv12() {
        ensure_mf();
        unsafe {
            let Some(mgr) = crate::mf_hw::dxgi_manager() else {
                eprintln!("no D3D11 hardware device — skipping");
                return;
            };
            let input = VideoInput::Path(fixture());
            let (reader, w, h) = match crate::mf_hw::open_nv12_reader(&input, &mgr, None) {
                Ok(x) => x,
                Err(e) => {
                    eprintln!("hw NV12 open failed ({e}) — skipping (fallback covers this)");
                    return;
                }
            };
            assert!(w > 0 && h > 0 && w % 2 == 0 && h % 2 == 0, "{w}x{h}");
            let mut kind = OutKind::Nv12;
            let mut frames = 0usize;
            for _ in 0..40 {
                match read_one(&reader, w, h, &mut kind) {
                    Ok(Read1::Frame { pixels, .. }) => {
                        assert_eq!(
                            pixels.len(),
                            PixelFormat::Nv12.frame_bytes(w, h),
                            "packed NV12 planes"
                        );
                        frames += 1;
                        if frames >= 5 {
                            break;
                        }
                    }
                    Ok(Read1::Eos) => break,
                    Ok(Read1::Gap) => {}
                    Err(e) => panic!("hw read failed: {e}"),
                }
            }
            assert!(frames >= 1, "the hw path produced NV12 frames");
            retire_reader(reader);
        }
    }

    #[test]
    fn a_bad_path_fails_cleanly() {
        let (_msgs, events) = spawn(std::path::PathBuf::from(r"C:\nope\missing.mp4"));
        match events.recv_timeout(Duration::from_secs(10)).expect("event") {
            VideoProducerEvent::Failed { session_id, .. } => assert_eq!(session_id, SID),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// Archive playback's core claim: the producer streams, seeks (a reopen over the
    /// SAME shared bytes — no path anywhere), and reports EOS from an in-RAM
    /// container exactly like a file.
    #[test]
    fn producer_streams_and_seeks_from_in_ram_bytes() {
        let data = std::sync::Arc::new(std::fs::read(fixture()).expect("fixture bytes"));
        let (msgs, events) = spawn_input(VideoInput::Bytes {
            data,
            name: "sub/clip.mp4".into(),
        });
        match events
            .recv_timeout(Duration::from_secs(10))
            .expect("opened")
        {
            VideoProducerEvent::Opened { width, height, .. } => {
                assert_eq!((width, height), (64, 64), "bytes open ≡ path open");
            }
            other => panic!("expected Opened, got {other:?}"),
        }
        // One sequential frame…
        msgs.send(VideoProducerMsg::Credit).unwrap();
        match events.recv_timeout(Duration::from_secs(10)).expect("frame") {
            VideoProducerEvent::Frame(f) => {
                assert!(f.is_well_formed());
                assert_eq!(f.pts, Duration::ZERO);
            }
            other => panic!("expected a frame, got {other:?}"),
        }
        // …then a seek, which exercises the fresh-reader reopen from the bytes.
        let g1 = SeekGeneration(1);
        msgs.send(VideoProducerMsg::SeekTo {
            target: Duration::from_millis(500),
            generation: g1,
        })
        .unwrap();
        msgs.send(VideoProducerMsg::Credit).unwrap();
        match events
            .recv_timeout(Duration::from_secs(10))
            .expect("landing")
        {
            VideoProducerEvent::Frame(f) => {
                assert_eq!(f.seek_generation, g1);
                assert!(f.pts >= Duration::from_millis(500), "landed at {:?}", f.pts);
            }
            other => panic!("expected the landing frame, got {other:?}"),
        }
    }

    /// Hostile in-RAM bytes fail with a structured error, never a hang or crash.
    #[test]
    fn garbage_bytes_fail_cleanly() {
        let (_msgs, events) = spawn_input(VideoInput::Bytes {
            data: std::sync::Arc::new(vec![0x55u8; 4096]),
            name: "junk.mp4".into(),
        });
        match events.recv_timeout(Duration::from_secs(10)).expect("event") {
            VideoProducerEvent::Failed { session_id, .. } => assert_eq!(session_id, SID),
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
