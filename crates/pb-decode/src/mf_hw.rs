//! Hardware video decode support (task 79.10): the D3D11 device manager Media
//! Foundation needs for GPU-accelerated (NVDEC/AMF/QuickSync) decode, the NV12
//! output negotiation, and the **pixel-rate policy** that decides when the
//! hardware path pays.
//!
//! Measured (`.taskmaster/docs/79.10-gpu-decode-spike.md`, RTX 5090): the hw path
//! has a ~6 ms/frame flat `Lock2DSize` readback sync — a ~166–195 fps ceiling at
//! *any* resolution. That beats software 3× at 4K60 (72 fps ceiling) but *loses*
//! to it on small clips (software: 480–1190 fps). Hence the policy: hardware for
//! heavy SDR clips, today's software RGB32 path — byte-for-byte unchanged — for
//! everything else and for any hw setup failure.

#![cfg(windows)]

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Multithread, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer2, IMFAttributes, IMFDXGIDeviceManager, IMFSample, IMFSourceReader,
    MF2DBuffer_LockFlags_Read, MFCreateAttributes, MFCreateDXGIDeviceManager, MFCreateMediaType,
    MFCreateSourceReaderFromByteStream, MFCreateSourceReaderFromURL, MFMediaType_Video,
    MFVideoFormat_NV12, MFVideoFormat_P010, MFVideoTransFunc_2084, MFVideoTransFunc_HLG,
    MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION,
    MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX,
    MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SOURCE_READER_ALL_STREAMS,
    MF_SOURCE_READER_D3D_MANAGER, MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM,
};

use crate::video::{VideoInput, VideoTransfer, YuvMatrix};

/// The pixel-rate threshold (native `w·h·fps`) above which hardware decode wins.
/// Sits just past 4K30 (248.8 M px/s) — measured "comfortable" on software, and
/// below it software's 480+ fps ceiling beats the hw readback's flat ~170 fps;
/// 4K60 (498 M px/s) is where software can't hold real time.
pub(crate) const HW_PIXEL_RATE: f64 = 250_000_000.0;

/// The pure policy: does this stream's decode load want the hardware path?
/// `fps == 0` (no rate metadata) assumes 30. Pure and unit-tested; the env
/// override and setup fallbacks wrap it at the call site.
pub(crate) fn pixel_rate_wants_hw(width: u32, height: u32, fps: f64) -> bool {
    let fps = if fps > 0.0 { fps } else { 30.0 };
    width as f64 * height as f64 * fps >= HW_PIXEL_RATE
}

/// `PB_VIDEO_FORCE_HW=1|0` overrides the policy (A/B testing + the integration
/// tests, which force the hw path onto the tiny committed fixture).
pub(crate) fn hw_override() -> Option<bool> {
    match std::env::var("PB_VIDEO_FORCE_HW").ok().as_deref() {
        Some("1") => Some(true),
        Some("0") => Some(false),
        _ => None,
    }
}

/// A D3D11 hardware device (video support on, multithread-protected — MF decodes
/// on its own threads) wrapped in the DXGI device manager the source reader
/// wants. `None` on any failure (VM, no adapter, driver trouble) — the caller
/// falls back to software. Created once per producer run and reused across seek
/// reader recreation; COM refcounting keeps it valid while retiring readers
/// still reference it (see the plan's lifetime-ordering note).
pub(crate) unsafe fn dxgi_manager() -> Option<IMFDXGIDeviceManager> {
    let inner = || -> windows::core::Result<IMFDXGIDeviceManager> {
        let mut device: Option<ID3D11Device> = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            windows::Win32::Foundation::HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
        let device = device.expect("device out param set on success");
        let mt: ID3D11Multithread = device.cast()?;
        let _ = mt.SetMultithreadProtected(true);
        let mut token = 0u32;
        let mut mgr = None;
        MFCreateDXGIDeviceManager(&mut token, &mut mgr)?;
        let mgr = mgr.expect("manager out param set on success");
        mgr.ResetDevice(&device, token)?;
        Ok(mgr)
    };
    inner().ok()
}

/// Create a source reader in the **hardware** configuration — the production
/// attributes (advanced processing for rotation/scaling, video-only stream
/// selection) plus the DXGI manager + hardware transforms — with the output
/// format left un-negotiated. Shared by the NV12 and P010 openers.
unsafe fn hw_reader(
    input: &VideoInput,
    manager: &IMFDXGIDeviceManager,
) -> windows::core::Result<IMFSourceReader> {
    let mut attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attrs, 3)?;
    let attrs = attrs.expect("MFCreateAttributes succeeded");
    attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
    attrs.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, manager)?;
    attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
    let reader: IMFSourceReader = match crate::mf_stream::ReaderSource::new(input)? {
        crate::mf_stream::ReaderSource::Url(url) => MFCreateSourceReaderFromURL(&url, &attrs)?,
        crate::mf_stream::ReaderSource::Stream(bs) => {
            MFCreateSourceReaderFromByteStream(&bs, &attrs)?
        }
    };
    reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
    reader.SetStreamSelection(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, true)?;
    Ok(reader)
}

/// Open a hardware reader negotiating **NV12** output at the fitted size (falling
/// back to native size). Errors bubble so the caller can fall back to software.
pub(crate) unsafe fn open_nv12_reader(
    input: &VideoInput,
    manager: &IMFDXGIDeviceManager,
    fit: Option<(u32, u32)>,
) -> windows::core::Result<(IMFSourceReader, u32, u32)> {
    let reader = hw_reader(input, manager)?;
    let (w, h) = match fit {
        Some(dims) => {
            negotiate_nv12(&reader, Some(dims)).or_else(|_| negotiate_nv12(&reader, None))?
        }
        None => negotiate_nv12(&reader, None)?,
    };
    Ok((reader, w, h))
}

/// Open a hardware reader negotiating **P010** output (HDR) — the
/// [`open_nv12_reader`] analog at 10-bit, used when the source is PQ/HLG and the
/// renderer supports P010.
pub(crate) unsafe fn open_p010_reader(
    input: &VideoInput,
    manager: &IMFDXGIDeviceManager,
    fit: Option<(u32, u32)>,
) -> windows::core::Result<(IMFSourceReader, u32, u32)> {
    let reader = hw_reader(input, manager)?;
    let (w, h) = match fit {
        Some(dims) => {
            negotiate_p010(&reader, Some(dims)).or_else(|_| negotiate_p010(&reader, None))?
        }
        None => negotiate_p010(&reader, None)?,
    };
    Ok((reader, w, h))
}

/// Negotiate NV12 output; `fit` asks the (GPU) video processor to scale. NV12
/// requires even dimensions — MF's processor emits even, and the caller treats
/// odd output as a negotiation failure (→ software fallback).
pub(crate) unsafe fn negotiate_nv12(
    reader: &IMFSourceReader,
    fit: Option<(u32, u32)>,
) -> windows::core::Result<(u32, u32)> {
    let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let out = MFCreateMediaType()?;
    out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
    if let Some((w, h)) = fit {
        // Even-align what we ask for; NV12 subsamples chroma 2×2.
        out.SetUINT64(
            &MF_MT_FRAME_SIZE,
            (((w & !1) as u64) << 32) | (h & !1) as u64,
        )?;
    }
    reader.SetCurrentMediaType(video, None, &out)?;
    let cur = reader.GetCurrentMediaType(video)?;
    let packed = cur.GetUINT64(&MF_MT_FRAME_SIZE)?;
    Ok(((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32))
}

/// Negotiate **P010** (10-bit 4:2:0) output; the [`negotiate_nv12`] analog for HDR
/// (PQ/HLG) sources. MF passes the PQ/HLG-encoded 10-bit YUV through unconverted
/// (verified byte-exact vs FFmpeg's `p010le`), so the renderer applies the EOTF +
/// primaries in-shader. Even dimensions required.
pub(crate) unsafe fn negotiate_p010(
    reader: &IMFSourceReader,
    fit: Option<(u32, u32)>,
) -> windows::core::Result<(u32, u32)> {
    let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let out = MFCreateMediaType()?;
    out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_P010)?;
    if let Some((w, h)) = fit {
        out.SetUINT64(
            &MF_MT_FRAME_SIZE,
            (((w & !1) as u64) << 32) | (h & !1) as u64,
        )?;
    }
    reader.SetCurrentMediaType(video, None, &out)?;
    let cur = reader.GetCurrentMediaType(video)?;
    let packed = cur.GetUINT64(&MF_MT_FRAME_SIZE)?;
    Ok(((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32))
}

/// The stream's native colorimetry — YUV matrix, range, **transfer** (SDR / PQ /
/// HLG), and CICP colour-primaries — read from the **native** media type. The
/// transfer + primaries drive the HDR `VideoColorInfo` for the P010 path; MF
/// strips those off the negotiated P010 *output* type (verified: it reports no
/// transfer/primaries), so the producer carries them itself. Missing attributes
/// take the broadcast conventions: limited range, BT.709 for ≥720p / BT.601 below,
/// SDR transfer, BT.709 primaries.
pub(crate) struct NativeColor {
    pub yuv_matrix: YuvMatrix,
    pub full_range: bool,
    pub transfer: VideoTransfer,
    /// CICP colour-primaries code (1 = BT.709, 9 = BT.2020, …) for `ColorTransform`.
    pub primaries: u8,
}

pub(crate) unsafe fn native_color(reader: &IMFSourceReader, height: u32) -> NativeColor {
    let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let Ok(native) = reader.GetNativeMediaType(video, 0) else {
        return NativeColor {
            yuv_matrix: default_matrix(height),
            full_range: false,
            transfer: VideoTransfer::SrgbLike,
            primaries: 1,
        };
    };
    let yuv_matrix = match native.GetUINT32(&MF_MT_YUV_MATRIX) {
        Ok(2) => YuvMatrix::Bt601,
        Ok(1) | Ok(3) => YuvMatrix::Bt709,
        Ok(4) | Ok(5) => YuvMatrix::Bt2020,
        _ => default_matrix(height),
    };
    let full_range = matches!(native.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE), Ok(1));
    let transfer = match native.GetUINT32(&MF_MT_TRANSFER_FUNCTION) {
        Ok(t) if t == MFVideoTransFunc_2084.0 as u32 => VideoTransfer::Pq,
        Ok(t) if t == MFVideoTransFunc_HLG.0 as u32 => VideoTransfer::Hlg,
        _ => VideoTransfer::SrgbLike,
    };
    let primaries = mf_primaries_to_cicp(native.GetUINT32(&MF_MT_VIDEO_PRIMARIES).unwrap_or(0));
    NativeColor {
        yuv_matrix,
        full_range,
        transfer,
        primaries,
    }
}

/// MFVideoPrimaries enum → CICP (H.273) colour-primaries code. The HDR case that
/// matters is BT.2020 (both 9); the rest map to their CICP equivalents, defaulting
/// to BT.709 (1).
fn mf_primaries_to_cicp(p: u32) -> u8 {
    match p {
        2 => 1,   // MFVideoPrimaries_BT709      → CICP 1
        4 => 5,   // BT470_2_SysBG (PAL)         → CICP 5
        5 => 6,   // SMPTE170M (NTSC)            → CICP 6
        6 => 7,   // SMPTE240M                   → CICP 7
        9 => 9,   // BT2020                      → CICP 9
        11 => 12, // DCI_P3 → the common display P3-D65 (CICP 12)
        _ => 1,   // unknown → BT.709
    }
}

/// Copy one NV12 sample into a tightly packed `w·h·3/2` buffer (Y plane, then
/// the interleaved UV plane) — the spike-measured `Lock2DSize` readback that
/// skips `ConvertToContiguousBuffer`'s internal copy. Falls back to the
/// contiguous path for a buffer that isn't 2D (or reports a bottom-up pitch,
/// which real NV12 decoders never emit).
pub(crate) unsafe fn sample_to_nv12(
    sample: &IMFSample,
    w: u32,
    h: u32,
) -> windows::core::Result<Vec<u8>> {
    let (wu, hu) = (w as usize, h as usize);
    let mut out = vec![0u8; wu * hu * 3 / 2];
    let buffer = sample.GetBufferByIndex(0)?;
    if let Ok(b2d) = buffer.cast::<IMF2DBuffer2>() {
        let mut scan0: *mut u8 = std::ptr::null_mut();
        let mut pitch: i32 = 0;
        let mut start: *mut u8 = std::ptr::null_mut();
        let mut len: u32 = 0;
        b2d.Lock2DSize(
            MF2DBuffer_LockFlags_Read,
            &mut scan0,
            &mut pitch,
            &mut start,
            &mut len,
        )?;
        if pitch > 0 {
            let pitch = pitch as isize;
            for y in 0..hu {
                let row = std::slice::from_raw_parts(scan0.offset(y as isize * pitch), wu);
                out[y * wu..(y + 1) * wu].copy_from_slice(row);
            }
            let uv_base = scan0.offset(hu as isize * pitch);
            let uv_out = &mut out[wu * hu..];
            for y in 0..hu / 2 {
                let row = std::slice::from_raw_parts(uv_base.offset(y as isize * pitch), wu);
                uv_out[y * wu..(y + 1) * wu].copy_from_slice(row);
            }
            b2d.Unlock2D()?;
            return Ok(out);
        }
        b2d.Unlock2D()?;
        // Negative pitch: fall through to the contiguous copy below.
    }
    let contiguous = sample.ConvertToContiguousBuffer()?;
    let mut data: *mut u8 = std::ptr::null_mut();
    let mut len: u32 = 0;
    contiguous.Lock(&mut data, None, Some(&mut len))?;
    let n = out.len().min(len as usize);
    out[..n].copy_from_slice(std::slice::from_raw_parts(data, n));
    contiguous.Unlock()?;
    Ok(out)
}

/// Copy one **P010** sample into a tightly packed `w·h·3` buffer (10-bit-in-16 Y
/// plane, then the interleaved UV plane) — the [`sample_to_nv12`] analog at 2 bytes
/// per sample. P010 is high-aligned 10-bit and the renderer's `planar_range` ten-bit
/// path expects exactly that, so no bit-shift here (byte-matches FFmpeg's `p010le`).
pub(crate) unsafe fn sample_to_p010(
    sample: &IMFSample,
    w: u32,
    h: u32,
) -> windows::core::Result<Vec<u8>> {
    let (wu, hu) = (w as usize, h as usize);
    let row = wu * 2; // bytes per Y (and per interleaved-UV) row
    let mut out = vec![0u8; wu * hu * 3];
    let buffer = sample.GetBufferByIndex(0)?;
    if let Ok(b2d) = buffer.cast::<IMF2DBuffer2>() {
        let mut scan0: *mut u8 = std::ptr::null_mut();
        let mut pitch: i32 = 0;
        let mut start: *mut u8 = std::ptr::null_mut();
        let mut len: u32 = 0;
        b2d.Lock2DSize(
            MF2DBuffer_LockFlags_Read,
            &mut scan0,
            &mut pitch,
            &mut start,
            &mut len,
        )?;
        if pitch > 0 {
            let pitch = pitch as isize;
            for y in 0..hu {
                let src = std::slice::from_raw_parts(scan0.offset(y as isize * pitch), row);
                out[y * row..(y + 1) * row].copy_from_slice(src);
            }
            let uv_base = scan0.offset(hu as isize * pitch);
            let uv_out = &mut out[wu * hu * 2..];
            for y in 0..hu / 2 {
                let src = std::slice::from_raw_parts(uv_base.offset(y as isize * pitch), row);
                uv_out[y * row..(y + 1) * row].copy_from_slice(src);
            }
            b2d.Unlock2D()?;
            return Ok(out);
        }
        b2d.Unlock2D()?;
        // Negative pitch: fall through to the contiguous copy below.
    }
    let contiguous = sample.ConvertToContiguousBuffer()?;
    let mut data: *mut u8 = std::ptr::null_mut();
    let mut len: u32 = 0;
    contiguous.Lock(&mut data, None, Some(&mut len))?;
    let n = out.len().min(len as usize);
    out[..n].copy_from_slice(std::slice::from_raw_parts(data, n));
    contiguous.Unlock()?;
    Ok(out)
}

/// The height heuristic when the container doesn't say (the industry default).
fn default_matrix(height: u32) -> YuvMatrix {
    if height >= 720 {
        YuvMatrix::Bt709
    } else {
        YuvMatrix::Bt601
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The policy's measured boundaries: 4K30+ wants hardware, 1080p60 and
    /// below stays software, and missing fps metadata assumes 30.
    #[test]
    fn pixel_rate_policy_matches_the_measured_boundaries() {
        assert!(pixel_rate_wants_hw(3840, 2160, 60.0), "4K60 → hw");
        assert!(pixel_rate_wants_hw(3840, 2160, 50.0), "4K50 → hw");
        // 4K30 (248.8 M px/s) sits deliberately just under the line: measured
        // comfortable on software, and software wins below the threshold.
        assert!(
            !pixel_rate_wants_hw(3840, 2160, 30.0),
            "4K30 stays software"
        );
        assert!(!pixel_rate_wants_hw(1920, 1080, 60.0), "1080p60 → software");
        assert!(
            !pixel_rate_wants_hw(1920, 1440, 46.4),
            "the VFR clip → software"
        );
        assert!(!pixel_rate_wants_hw(1102, 720, 29.97), "the MKV → software");
        // No fps metadata assumes 30: 4K unknown-rate stays software too.
        assert!(!pixel_rate_wants_hw(3840, 2160, 0.0));
        assert!(pixel_rate_wants_hw(7680, 4320, 0.0), "8K any-rate → hw");
    }

    #[test]
    fn default_matrix_uses_the_height_heuristic() {
        assert_eq!(default_matrix(2160), YuvMatrix::Bt709);
        assert_eq!(default_matrix(720), YuvMatrix::Bt709);
        assert_eq!(default_matrix(480), YuvMatrix::Bt601);
    }

    #[test]
    fn mf_primaries_map_to_cicp() {
        assert_eq!(mf_primaries_to_cicp(2), 1, "BT709");
        assert_eq!(mf_primaries_to_cicp(9), 9, "BT2020 (the HDR case)");
        assert_eq!(mf_primaries_to_cicp(11), 12, "DCI_P3 -> P3-D65");
        assert_eq!(mf_primaries_to_cicp(0), 1, "unknown -> BT709");
    }
}
