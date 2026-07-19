//! Task #79 phase-0 spike harness: measure what Media Foundation's Source Reader gives
//! us for general video playback, before the `VideoSession` producer is built.
//!
//!   cargo run --release -p pb-decode --example video_probe -- spike <file> [--fit W H] [--frames N] [--dump <dir>]
//!   cargo run --release -p pb-decode --example video_probe -- sweep <file>...
//!
//! **spike** answers, per clip, the plan's phase-0 questions:
//!   - Does the reader accept a *fitted* RGB32 output size (MF video processor scales)
//!     and what does that do to ms/frame vs native-size RGB32 + our own downscale?
//!   - Stream deselection (deselect all, select video only — unread selected streams
//!     queue samples indefinitely per MF docs).
//!   - Real PTS: first timestamp, monotonicity, min/max delta (VFR detection).
//!   - Seek: SetCurrentPosition to 50%, decode forward to the target — landing cost.
//!   - Cancellation: how long dropping the reader blocks.
//!   - Native color (primaries/transfer/matrix/range) + rotation + audio presence.
//!
//! **sweep** is the container-capability probe: for each file, can MF open it, select
//! video, negotiate RGB32, and produce one frame? Prints OK/err per file — this is the
//! measurement the format matrix ships from (MKV via the native byte-stream handler,
//! webm with/without the Web Media Extensions, …).

#[cfg(not(windows))]
fn main() {
    eprintln!("video_probe spikes the Windows Media Foundation path.");
}

#[cfg(windows)]
fn main() {
    win::main()
}

#[cfg(windows)]
mod win {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use windows::core::{GUID, HSTRING};
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::StructuredStorage::{
        PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
    };
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
    use windows::Win32::System::Variant::{VT_I8, VT_UI8};

    /// A VT_I8 PROPVARIANT holding a 100 ns media position — what
    /// `IMFSourceReader::SetCurrentPosition` wants. Numeric variants need no drop.
    fn propvariant_i8(value: i64) -> PROPVARIANT {
        PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                    vt: VT_I8,
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: PROPVARIANT_0_0_0 { hVal: value },
                }),
            },
        }
    }

    /// Read a VT_UI8/VT_I8 PROPVARIANT (MF duration attributes are VT_UI8).
    unsafe fn propvariant_u64(pv: &PROPVARIANT) -> Option<u64> {
        let inner = &pv.Anonymous.Anonymous;
        match inner.vt {
            VT_UI8 => Some(inner.Anonymous.uhVal),
            VT_I8 => u64::try_from(inner.Anonymous.hVal).ok(),
            _ => None,
        }
    }

    pub fn main() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let _ = MFStartup(MF_VERSION, MFSTARTUP_LITE);
        }
        let args: Vec<String> = std::env::args().skip(1).collect();
        match args.first().map(String::as_str) {
            Some("spike") => spike(&args[1..]),
            Some("sweep") => sweep(&args[1..]),
            Some("copybench") => copybench(&args[1..]),
            Some("seekbench") => seekbench(&args[1..]),
            Some("hdrprobe") => hdrprobe(&args[1..]),
            _ => {
                eprintln!(
                    "usage: video_probe spike <file> [--fit W H] [--frames N] [--dump <dir>]"
                );
                eprintln!("       video_probe sweep <file>...");
                eprintln!("       video_probe copybench <file> [--fit W H] [--frames N]");
                eprintln!("       video_probe seekbench <file> [--fit W H] [--frac 0.5]");
                std::process::exit(2);
            }
        }
    }

    // ---------------------------------------------------------------- helpers

    fn hr_name(e: &windows::core::Error) -> String {
        let code = e.code().0 as u32;
        let known = match code {
            0xC00D5212 => " (MF_E_TOPO_CODEC_NOT_FOUND — codec missing)",
            0xC00D36C4 => " (MF_E_UNSUPPORTED_BYTESTREAM_TYPE — no container handler)",
            0xC00D36B4 => " (MF_E_INVALIDMEDIATYPE)",
            0xC00D36E6 => " (MF_E_ATTRIBUTENOTFOUND)",
            0x80070002 => " (file not found)",
            _ => "",
        };
        format!("0x{code:08X}{known}{}", {
            let m = e.message();
            if m.is_empty() {
                String::new()
            } else {
                format!(" {m}")
            }
        })
    }

    fn codec_name(sub: &GUID) -> String {
        let named = [
            (MFVideoFormat_H264, "H.264"),
            (MFVideoFormat_HEVC, "HEVC"),
            (MFVideoFormat_HEVC_ES, "HEVC-ES"),
            (MFVideoFormat_VP80, "VP8"),
            (MFVideoFormat_VP90, "VP9"),
            (MFVideoFormat_AV1, "AV1"),
            (MFVideoFormat_WMV1, "WMV1"),
            (MFVideoFormat_WMV2, "WMV2"),
            (MFVideoFormat_WMV3, "WMV3"),
            (MFVideoFormat_WVC1, "VC-1"),
            (MFVideoFormat_MPEG2, "MPEG-2"),
            (MFVideoFormat_MP4V, "MPEG-4 pt2"),
            (MFVideoFormat_MP43, "MS MPEG-4 v3"),
            (MFVideoFormat_MJPG, "MJPEG"),
            (MFVideoFormat_H263, "H.263"),
            (MFVideoFormat_DV25, "DV25"),
            (MFVideoFormat_DVSD, "DVSD"),
        ];
        for (g, n) in named {
            if *sub == g {
                return n.to_string();
            }
        }
        format!("{sub:?}")
    }

    /// Open a source reader for `path` with advanced video processing, all streams
    /// deselected, and only the first video stream selected (`select_video`).
    unsafe fn open_reader(
        path: &Path,
        select_video: bool,
    ) -> windows::core::Result<IMFSourceReader> {
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1)?;
        let attrs = attrs.expect("attrs");
        attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
        let reader: IMFSourceReader =
            MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attrs)?;
        if select_video {
            // The plan's guardrail: deselect everything, then select video only —
            // a selected-but-unread stream queues samples indefinitely.
            reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
            reader.SetStreamSelection(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, true)?;
        }
        Ok(reader)
    }

    /// Negotiate RGB32 output; `fit` = Some((w,h)) asks the MF video processor to scale
    /// to that exact size (caller pre-computes aspect). Returns negotiated (w, h, stride).
    unsafe fn negotiate_rgb32(
        reader: &IMFSourceReader,
        fit: Option<(u32, u32)>,
    ) -> windows::core::Result<(u32, u32, i32)> {
        let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let out = MFCreateMediaType()?;
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        if let Some((w, h)) = fit {
            out.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64)?;
        }
        reader.SetCurrentMediaType(video, None, &out)?;
        let cur = reader.GetCurrentMediaType(video)?;
        let packed = cur.GetUINT64(&MF_MT_FRAME_SIZE)?;
        let (w, h) = ((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32);
        let stride = cur
            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
            .map(|s| s as i32)
            .unwrap_or((w * 4) as i32);
        Ok((w, h, stride))
    }

    struct NativeInfo {
        codec: String,
        w: u32,
        h: u32,
        fps: f64,
        duration: Option<Duration>,
        rotation: Option<u32>,
        primaries: Option<u32>,
        transfer: Option<u32>,
        matrix: Option<u32>,
        range: Option<u32>,
        has_audio: bool,
    }

    unsafe fn native_info(reader: &IMFSourceReader) -> windows::core::Result<NativeInfo> {
        let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let native = reader.GetNativeMediaType(video, 0)?;
        let sub = native.GetGUID(&MF_MT_SUBTYPE)?;
        let packed = native.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
        let (w, h) = ((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32);
        let fps = native
            .GetUINT64(&MF_MT_FRAME_RATE)
            .map(|fr| {
                let (n, d) = ((fr >> 32) as u32, (fr & 0xFFFF_FFFF) as u32);
                if d == 0 {
                    0.0
                } else {
                    n as f64 / d as f64
                }
            })
            .unwrap_or(0.0);
        let duration = reader
            .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
            .ok()
            .and_then(|pv| propvariant_u64(&pv))
            .map(|hns| Duration::from_nanos(hns * 100));
        let has_audio = reader
            .GetNativeMediaType(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32, 0)
            .is_ok();
        Ok(NativeInfo {
            codec: codec_name(&sub),
            w,
            h,
            fps,
            duration,
            rotation: native.GetUINT32(&MF_MT_VIDEO_ROTATION).ok(),
            primaries: native.GetUINT32(&MF_MT_VIDEO_PRIMARIES).ok(),
            transfer: native.GetUINT32(&MF_MT_TRANSFER_FUNCTION).ok(),
            matrix: native.GetUINT32(&MF_MT_YUV_MATRIX).ok(),
            range: native.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE).ok(),
            has_audio,
        })
    }

    /// Pump up to `max_frames` samples; returns (pts list, per-frame read ms, copy ms,
    /// eos, media-type-change count). `copy` = ConvertToContiguousBuffer + Lock + BGRX→
    /// RGBA copy into a reused buffer (what the real producer must do per frame).
    unsafe fn pump(
        reader: &IMFSourceReader,
        w: u32,
        h: u32,
        stride: i32,
        max_frames: usize,
        dump_first: Option<&Path>,
    ) -> windows::core::Result<PumpStats> {
        let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let mut pts = Vec::new();
        let mut read_ms = Vec::new();
        let mut copy_ms = Vec::new();
        let mut eos = false;
        let mut type_changes = 0usize;
        let mut rgba = vec![0u8; w as usize * h as usize * 4];
        while pts.len() < max_frames {
            let t0 = Instant::now();
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample: Option<IMFSample> = None;
            reader.ReadSample(
                video,
                0,
                None,
                Some(&mut flags),
                Some(&mut ts),
                Some(&mut sample),
            )?;
            if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
                eos = true;
                break;
            }
            if flags & (MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32) != 0 {
                type_changes += 1;
            }
            let Some(sample) = sample else { continue };
            read_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
            pts.push(ts);

            let t1 = Instant::now();
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut data, None, Some(&mut len))?;
            let src = std::slice::from_raw_parts(data, len as usize);
            let (wu, hu) = (w as usize, h as usize);
            let row_bytes = wu * 4;
            let abs_stride = stride.unsigned_abs() as usize;
            for y in 0..hu {
                let src_y = if stride < 0 { hu - 1 - y } else { y };
                let Some(row) = src.get(src_y * abs_stride..src_y * abs_stride + row_bytes) else {
                    break;
                };
                let dst = &mut rgba[y * row_bytes..(y + 1) * row_bytes];
                for x in 0..wu {
                    dst[x * 4] = row[x * 4 + 2];
                    dst[x * 4 + 1] = row[x * 4 + 1];
                    dst[x * 4 + 2] = row[x * 4];
                    dst[x * 4 + 3] = 255;
                }
            }
            buffer.Unlock()?;
            copy_ms.push(t1.elapsed().as_secs_f64() * 1000.0);

            if pts.len() == 1 {
                if let Some(out) = dump_first {
                    let _ = image::save_buffer(out, &rgba, w, h, image::ColorType::Rgba8)
                        .map(|()| println!("    wrote {}", out.display()));
                }
            }
        }
        Ok(PumpStats {
            pts,
            read_ms,
            copy_ms,
            eos,
            type_changes,
        })
    }

    struct PumpStats {
        pts: Vec<i64>,
        read_ms: Vec<f64>,
        copy_ms: Vec<f64>,
        eos: bool,
        type_changes: usize,
    }

    fn pct(sorted: &[f64], p: f64) -> f64 {
        if sorted.is_empty() {
            return f64::NAN;
        }
        let i = ((sorted.len() - 1) as f64 * p).round() as usize;
        sorted[i]
    }

    fn stats_line(label: &str, ms: &[f64]) {
        let mut s = ms.to_vec();
        s.sort_by(f64::total_cmp);
        println!(
            "    {label}: n={} p50={:.2}ms p95={:.2}ms p99={:.2}ms max={:.2}ms",
            s.len(),
            pct(&s, 0.50),
            pct(&s, 0.95),
            pct(&s, 0.99),
            pct(&s, 1.0),
        );
    }

    // ------------------------------------------------------------- copybench
    //
    // Task 79.10 (GPU decode escalation): measure every rung's CPU side in one
    // run, per clip — the numbers that decide how far up the ladder to climb.
    //
    //   1. rgb32/contig  — the shipping path: ConvertToContiguousBuffer (MF's own
    //      internal copy) + Lock + the word-wise BGRX→RGBA swizzle.
    //   2. rgb32/lock2d  — rung 1: IMF2DBuffer2::Lock2DSize skips MF's internal
    //      copy; same swizzle from the pitched rows.
    //   3. nv12/lock2d   — rung 2's CPU side: NV12 output (12 bpp, no RGB
    //      conversion in MF) + Lock2D + plane memcpy — what the in-shader YUV
    //      path would ship over the bus.
    //   4. hw+nv12/lock2d — rung 3's CPU side: D3D11 device manager + hardware
    //      transforms (NVDEC) + NV12 + Lock2D readback + plane memcpy. Decode
    //      CPU should collapse; what remains is the readback + copy.

    fn copybench(args: &[String]) {
        let mut file = None;
        let mut fit = (3840u32, 2160u32);
        let mut frames = 240usize;
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--fit" => {
                    fit = (
                        it.next().and_then(|s| s.parse().ok()).unwrap_or(3840),
                        it.next().and_then(|s| s.parse().ok()).unwrap_or(2160),
                    )
                }
                "--frames" => frames = it.next().and_then(|s| s.parse().ok()).unwrap_or(240),
                f => file = Some(f.to_string()),
            }
        }
        let Some(file) = file else {
            eprintln!("copybench: no file given");
            std::process::exit(2);
        };
        let path = Path::new(&file);
        println!(
            "== copybench {file} (fit {}x{}, {frames} frames/mode)",
            fit.0, fit.1
        );
        unsafe {
            if let Err(e) = copybench_inner(path, fit, frames) {
                println!("  COPYBENCH FAILED: {}", hr_name(&e));
            }
        }
    }

    /// Which output subtype a bench mode negotiates.
    #[derive(Clone, Copy, PartialEq)]
    enum BenchSub {
        Rgb32,
        Nv12,
    }

    /// How a bench mode gets pixels out of the sample.
    #[derive(Clone, Copy, PartialEq)]
    enum BenchCopy {
        /// `ConvertToContiguousBuffer` + `Lock` (the shipping path — MF copies
        /// internally first).
        Contig,
        /// `IMF2DBuffer2::Lock2DSize` (read-only, no contiguous copy).
        Lock2d,
    }

    unsafe fn copybench_inner(
        path: &Path,
        fit_box: (u32, u32),
        frames: usize,
    ) -> windows::core::Result<()> {
        let reader = open_reader(path, true)?;
        let info = native_info(&reader)?;
        drop(reader);
        println!(
            "  native: {} {}x{} {:.3}fps dur={:?}",
            info.codec, info.w, info.h, info.fps, info.duration
        );
        let fitted = fit_dims(info.w, info.h, fit_box);

        for (label, sub, copy, hw) in [
            (
                "rgb32/contig   (ship today)",
                BenchSub::Rgb32,
                BenchCopy::Contig,
                false,
            ),
            (
                "rgb32/lock2d   (rung 1)    ",
                BenchSub::Rgb32,
                BenchCopy::Lock2d,
                false,
            ),
            (
                "nv12 /lock2d   (rung 2 cpu)",
                BenchSub::Nv12,
                BenchCopy::Lock2d,
                false,
            ),
            (
                "hw nv12/lock2d (rung 3 cpu)",
                BenchSub::Nv12,
                BenchCopy::Lock2d,
                true,
            ),
            // hw decode + GPU RGB convert + 32bpp readback: if this were fast
            // enough, no renderer work at all — measure it before ruling it out.
            (
                "hw rgb32/lock2d (no-shader)",
                BenchSub::Rgb32,
                BenchCopy::Lock2d,
                true,
            ),
        ] {
            match bench_mode(path, fitted, frames, sub, copy, hw) {
                Ok(()) => {}
                Err(e) => println!("  {label}: FAILED {}", hr_name(&e)),
            }
        }
        Ok(())
    }

    /// Run one mode: fresh reader, negotiate, one warmup frame (decoder init must
    /// not pollute the steady state), then pump `frames` timing read vs copy.
    unsafe fn bench_mode(
        path: &Path,
        fitted: (u32, u32),
        frames: usize,
        sub: BenchSub,
        copy: BenchCopy,
        hw: bool,
    ) -> windows::core::Result<()> {
        use windows::Win32::Media::MediaFoundation::{
            IMF2DBuffer2, MF2DBuffer_LockFlags_Read, MFVideoFormat_NV12,
        };
        let label = match (sub, copy, hw) {
            (BenchSub::Rgb32, BenchCopy::Contig, false) => "rgb32/contig   (ship today)",
            (BenchSub::Rgb32, BenchCopy::Lock2d, false) => "rgb32/lock2d   (rung 1)    ",
            (BenchSub::Nv12, BenchCopy::Lock2d, false) => "nv12 /lock2d   (rung 2 cpu)",
            (BenchSub::Nv12, BenchCopy::Lock2d, true) => "hw nv12/lock2d (rung 3 cpu)",
            (BenchSub::Rgb32, BenchCopy::Lock2d, true) => "hw rgb32/lock2d (no-shader)",
            _ => "custom",
        };

        // Reader: production config (advanced processing, video-only), plus the
        // DXGI device manager + hardware transforms for the hw mode.
        let manager = if hw { Some(dxgi_manager()?) } else { None };
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 3)?;
        let attrs = attrs.expect("attrs");
        attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
        if let Some(mgr) = &manager {
            attrs.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, mgr)?;
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        }
        let reader: IMFSourceReader =
            MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attrs)?;
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        reader.SetStreamSelection(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, true)?;

        // Negotiate the subtype at the fitted size; fall back to native size.
        let subtype = match sub {
            BenchSub::Rgb32 => &MFVideoFormat_RGB32,
            BenchSub::Nv12 => &MFVideoFormat_NV12,
        };
        let (w, h, stride, honored) = match negotiate_sub(&reader, subtype, Some(fitted), sub) {
            Ok((w, h, s)) => (w, h, s, (w, h) == fitted),
            Err(_) => {
                let (w, h, s) = negotiate_sub(&reader, subtype, None, sub)?;
                (w, h, s, false)
            }
        };

        // Output buffer: RGBA8 for RGB32 modes, packed Y+UV planes for NV12.
        let out_len = match sub {
            BenchSub::Rgb32 => w as usize * h as usize * 4,
            BenchSub::Nv12 => w as usize * h as usize * 3 / 2,
        };
        let mut out = vec![0u8; out_len];

        let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let mut read_ms: Vec<f64> = Vec::with_capacity(frames);
        let mut copy_ms: Vec<f64> = Vec::with_capacity(frames);
        let mut warmed = false;
        let mut got = 0usize;
        let t_wall = Instant::now();
        let mut wall_after_warmup = t_wall;
        while got < frames {
            let t0 = Instant::now();
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample: Option<IMFSample> = None;
            reader.ReadSample(
                video,
                0,
                None,
                Some(&mut flags),
                Some(&mut ts),
                Some(&mut sample),
            )?;
            if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
                break;
            }
            let Some(sample) = sample else { continue };
            let read_t = t0.elapsed().as_secs_f64() * 1000.0;

            let t1 = Instant::now();
            match copy {
                BenchCopy::Contig => {
                    let buffer = sample.ConvertToContiguousBuffer()?;
                    let mut data: *mut u8 = std::ptr::null_mut();
                    let mut len = 0u32;
                    buffer.Lock(&mut data, None, Some(&mut len))?;
                    let src = std::slice::from_raw_parts(data, len as usize);
                    swizzle_bgrx_rows(src, stride, w, h, &mut out);
                    buffer.Unlock()?;
                }
                BenchCopy::Lock2d => {
                    use windows::core::Interface;
                    let buffer = sample.GetBufferByIndex(0)?;
                    let b2d: IMF2DBuffer2 = buffer.cast()?;
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
                    match sub {
                        BenchSub::Rgb32 => swizzle_bgrx_2d(scan0, pitch, w, h, &mut out),
                        BenchSub::Nv12 => copy_nv12_2d(scan0, pitch, w, h, &mut out),
                    }
                    b2d.Unlock2D()?;
                }
            }
            let copy_t = t1.elapsed().as_secs_f64() * 1000.0;

            if !warmed {
                // Frame 0 carries decoder/pipeline init — keep it out of the
                // steady-state stats (it's reported via P→first-frame instead).
                warmed = true;
                wall_after_warmup = Instant::now();
                continue;
            }
            read_ms.push(read_t);
            copy_ms.push(copy_t);
            got += 1;
        }
        let wall = wall_after_warmup.elapsed().as_secs_f64();
        let fps = if wall > 0.0 { got as f64 / wall } else { 0.0 };
        println!("  {label}: {w}x{h} (fitted={honored}) frames={got} ceiling≈{fps:.0} fps",);
        stats_line("read", &read_ms);
        stats_line("copy", &copy_ms);
        Ok(())
    }

    /// Negotiate `subtype` output; `fit` asks the video processor to scale.
    unsafe fn negotiate_sub(
        reader: &IMFSourceReader,
        subtype: &GUID,
        fit: Option<(u32, u32)>,
        sub: BenchSub,
    ) -> windows::core::Result<(u32, u32, i32)> {
        let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let out = MFCreateMediaType()?;
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        if let Some((w, h)) = fit {
            out.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64)?;
        }
        reader.SetCurrentMediaType(video, None, &out)?;
        let cur = reader.GetCurrentMediaType(video)?;
        let packed = cur.GetUINT64(&MF_MT_FRAME_SIZE)?;
        let (w, h) = ((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32);
        let default_stride = match sub {
            BenchSub::Rgb32 => (w * 4) as i32,
            BenchSub::Nv12 => w as i32,
        };
        let stride = cur
            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
            .map(|s| s as i32)
            .unwrap_or(default_stride);
        Ok((w, h, stride))
    }

    /// A D3D11 hardware device (video support on) wrapped in the DXGI device
    /// manager MF wants for hardware-accelerated decode (NVDEC on the 5090).
    unsafe fn dxgi_manager(
    ) -> windows::core::Result<windows::Win32::Media::MediaFoundation::IMFDXGIDeviceManager> {
        use windows::core::Interface;
        use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
        use windows::Win32::Graphics::Direct3D11::{
            D3D11CreateDevice, ID3D11Device, ID3D11Multithread, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
        };
        use windows::Win32::Media::MediaFoundation::MFCreateDXGIDeviceManager;

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
        // MF decodes on its own threads — the device must be multithread-safe.
        let mt: ID3D11Multithread = device.cast()?;
        let _ = mt.SetMultithreadProtected(true);
        let mut token = 0u32;
        let mut mgr = None;
        MFCreateDXGIDeviceManager(&mut token, &mut mgr)?;
        let mgr = mgr.expect("manager out param set on success");
        mgr.ResetDevice(&device, token)?;
        Ok(mgr)
    }

    /// The production BGRX→RGBA word-wise swizzle over a contiguous buffer
    /// (handles negative stride = bottom-up).
    fn swizzle_bgrx_rows(src: &[u8], stride: i32, w: u32, h: u32, out: &mut [u8]) {
        let (w, h) = (w as usize, h as usize);
        let row_bytes = w * 4;
        let abs_stride = stride.unsigned_abs() as usize;
        for y in 0..h {
            let src_y = if stride < 0 { h - 1 - y } else { y };
            let Some(row) = src.get(src_y * abs_stride..src_y * abs_stride + row_bytes) else {
                break;
            };
            swizzle_bgrx_row(row, &mut out[y * row_bytes..(y + 1) * row_bytes]);
        }
    }

    /// The same swizzle over a 2D-locked buffer (`scan0` = first display row,
    /// `pitch` may be negative — the pointer walk handles both).
    unsafe fn swizzle_bgrx_2d(scan0: *mut u8, pitch: i32, w: u32, h: u32, out: &mut [u8]) {
        let (w, h) = (w as usize, h as usize);
        let row_bytes = w * 4;
        for y in 0..h {
            let row_ptr = scan0.offset(y as isize * pitch as isize);
            let row = std::slice::from_raw_parts(row_ptr, row_bytes);
            swizzle_bgrx_row(row, &mut out[y * row_bytes..(y + 1) * row_bytes]);
        }
    }

    /// One row of the word-wise (LLVM-vectorizable) BGRX→RGBA swizzle — the same
    /// loop the production producer runs (`mf_video::sample_to_rgba`).
    fn swizzle_bgrx_row(row: &[u8], dst: &mut [u8]) {
        for (d, s) in dst.chunks_exact_mut(4).zip(row.chunks_exact(4)) {
            let bgrx = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
            let rgba = (bgrx & 0x0000_FF00)
                | ((bgrx & 0x0000_00FF) << 16)
                | ((bgrx & 0x00FF_0000) >> 16)
                | 0xFF00_0000;
            d.copy_from_slice(&rgba.to_le_bytes());
        }
    }

    /// Copy NV12 planes (Y then interleaved UV at `scan0 + pitch·h`) into a packed
    /// 12 bpp buffer — the bytes rung 2 would upload for the in-shader convert.
    unsafe fn copy_nv12_2d(scan0: *mut u8, pitch: i32, w: u32, h: u32, out: &mut [u8]) {
        let (w, h) = (w as usize, h as usize);
        let pitch_i = pitch as isize;
        for y in 0..h {
            let row = std::slice::from_raw_parts(scan0.offset(y as isize * pitch_i), w);
            out[y * w..(y + 1) * w].copy_from_slice(row);
        }
        let uv_base = scan0.offset(h as isize * pitch_i);
        let uv_out = &mut out[w * h..];
        for y in 0..h / 2 {
            let row = std::slice::from_raw_parts(uv_base.offset(y as isize * pitch_i), w);
            uv_out[y * w..(y + 1) * w].copy_from_slice(row);
        }
    }

    // ---------------------------------------------------------------- seekbench
    //
    // Task #4 (seek gap): the producer's recreate-seek opens a fresh reader
    // positioned at the target (spike E: ~0 ms), then decodes FORWARD discarding
    // frames to the landing — the cost is dominated by those run-up frames. This
    // A/Bs the run-up under {software RGB32, hardware NV12} × {convert every frame
    // (the old producer), convert only the landing frame (the fix)}, so the saving
    // from skipping the discarded frames' readback/swizzle is a measured number
    // per path. The hw path is the one 4K60 SDR clips actually take.

    fn seekbench(args: &[String]) {
        let mut file = None;
        let mut fit = (3840u32, 2160u32);
        let mut frac = 0.5f64;
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--fit" => {
                    fit = (
                        it.next().and_then(|s| s.parse().ok()).unwrap_or(3840),
                        it.next().and_then(|s| s.parse().ok()).unwrap_or(2160),
                    )
                }
                "--frac" => frac = it.next().and_then(|s| s.parse().ok()).unwrap_or(0.5),
                f => file = Some(f.to_string()),
            }
        }
        let Some(file) = file else {
            eprintln!("seekbench: no file given");
            std::process::exit(2);
        };
        let path = Path::new(&file);
        println!(
            "== seekbench {file} (fit {}x{}, seek to {:.0}%)",
            fit.0,
            fit.1,
            frac * 100.0
        );
        unsafe {
            if let Err(e) = seekbench_inner(path, fit, frac) {
                println!("  SEEKBENCH FAILED: {}", hr_name(&e));
            }
        }
    }

    unsafe fn seekbench_inner(
        path: &Path,
        fit_box: (u32, u32),
        frac: f64,
    ) -> windows::core::Result<()> {
        let reader = open_reader(path, true)?;
        let info = native_info(&reader)?;
        drop(reader);
        let Some(dur) = info.duration else {
            println!("  no duration — cannot seek");
            return Ok(());
        };
        let fitted = fit_dims(info.w, info.h, fit_box);
        let target_hns = ((dur.as_nanos() as f64 * frac) / 100.0) as i64;
        println!(
            "  native: {} {}x{} {:.3}fps dur={:?} → fitted {}x{}, target {:.1}s",
            info.codec,
            info.w,
            info.h,
            info.fps,
            info.duration,
            fitted.0,
            fitted.1,
            target_hns as f64 / 1e7,
        );
        // Each combo uses a FRESH reader (an equally cold decode pipeline), so the
        // run-up ms are comparable. convert-all = the old producer; skip = the fix.
        for hw in [false, true] {
            for convert in [true, false] {
                match seek_run_up(path, fitted, target_hns, hw, convert) {
                    Ok((discarded, open_ms, run_ms)) => println!(
                        "  {:<8} {:<12}: open+neg+pos={open_ms:.0}ms run-up={run_ms:.0}ms discarded={discarded}",
                        if hw { "hw nv12" } else { "sw rgb32" },
                        if convert { "convert-all" } else { "skip (fix)" },
                    ),
                    Err(e) => println!(
                        "  {:<8} {}: FAILED {}",
                        if hw { "hw nv12" } else { "sw rgb32" },
                        if convert { "convert-all" } else { "skip" },
                        hr_name(&e),
                    ),
                }
            }
        }
        Ok(())
    }

    /// Open a fresh reader (sw RGB32 or hw NV12), position at `target_hns`, and
    /// decode forward to the landing frame. When `convert`, every discarded run-up
    /// frame is read back/swizzled (the old producer); otherwise only the landing
    /// frame is (the task-#4 fix). Returns (discarded, open+negotiate+position ms,
    /// run-up ms).
    unsafe fn seek_run_up(
        path: &Path,
        fitted: (u32, u32),
        target_hns: i64,
        hw: bool,
        convert: bool,
    ) -> windows::core::Result<(u32, f64, f64)> {
        use windows::core::Interface;
        use windows::Win32::Media::MediaFoundation::{
            IMF2DBuffer2, MF2DBuffer_LockFlags_Read, MFVideoFormat_NV12, MFVideoFormat_RGB32,
        };
        let t_open = Instant::now();
        let manager = if hw { Some(dxgi_manager()?) } else { None };
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 3)?;
        let attrs = attrs.expect("attrs");
        attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
        if let Some(mgr) = &manager {
            attrs.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, mgr)?;
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        }
        let reader: IMFSourceReader =
            MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attrs)?;
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        reader.SetStreamSelection(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, true)?;
        let (sub, subtype) = if hw {
            (BenchSub::Nv12, &MFVideoFormat_NV12)
        } else {
            (BenchSub::Rgb32, &MFVideoFormat_RGB32)
        };
        let (w, h, stride) = negotiate_sub(&reader, subtype, Some(fitted), sub)
            .or_else(|_| negotiate_sub(&reader, subtype, None, sub))?;
        // Recreate strategy: position BEFORE the first read (spike E: ~0 ms).
        let pos = propvariant_i8(target_hns.max(0));
        reader.SetCurrentPosition(&GUID::zeroed(), &pos)?;
        let open_ms = t_open.elapsed().as_secs_f64() * 1000.0;

        let mut out = match sub {
            BenchSub::Rgb32 => vec![0u8; w as usize * h as usize * 4],
            BenchSub::Nv12 => vec![0u8; w as usize * h as usize * 3 / 2],
        };
        let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let t_run = Instant::now();
        let mut discarded = 0u32;
        for _ in 0..2400 {
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample: Option<IMFSample> = None;
            reader.ReadSample(
                video,
                0,
                None,
                Some(&mut flags),
                Some(&mut ts),
                Some(&mut sample),
            )?;
            if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
                break;
            }
            let Some(sample) = sample else { continue };
            let landing = ts >= target_hns;
            // Both modes convert the landing frame (it's displayed); convert-all
            // also converts the discarded run-up — the cost the fix removes.
            if convert || landing {
                match sub {
                    BenchSub::Rgb32 => {
                        let buffer = sample.ConvertToContiguousBuffer()?;
                        let mut data: *mut u8 = std::ptr::null_mut();
                        let mut len = 0u32;
                        buffer.Lock(&mut data, None, Some(&mut len))?;
                        let src = std::slice::from_raw_parts(data, len as usize);
                        swizzle_bgrx_rows(src, stride, w, h, &mut out);
                        buffer.Unlock()?;
                    }
                    BenchSub::Nv12 => {
                        let buffer = sample.GetBufferByIndex(0)?;
                        let b2d: IMF2DBuffer2 = buffer.cast()?;
                        let mut scan0: *mut u8 = std::ptr::null_mut();
                        let mut pitch: i32 = 0;
                        let mut startp: *mut u8 = std::ptr::null_mut();
                        let mut len2: u32 = 0;
                        b2d.Lock2DSize(
                            MF2DBuffer_LockFlags_Read,
                            &mut scan0,
                            &mut pitch,
                            &mut startp,
                            &mut len2,
                        )?;
                        copy_nv12_2d(scan0, pitch, w, h, &mut out);
                        b2d.Unlock2D()?;
                    }
                }
            }
            if landing {
                break;
            }
            discarded += 1;
        }
        let run_ms = t_run.elapsed().as_secs_f64() * 1000.0;
        Ok((discarded, open_ms, run_ms))
    }

    // ---------------------------------------------------------------- hdrprobe
    //
    // Task 79.10 Track B (Windows HDR→P010): the pivotal question is whether MF's
    // P010 output PASSES PQ/HLG through raw (our shader does the EOTF) or SDR-
    // converts it (double application). Negotiate P010 on the hw reader and print
    // the OUTPUT media type's transfer/primaries/matrix/range vs the native ones.
    // If the output keeps 2084/HLG, the shader-EOTF path is valid.

    fn hdrprobe(args: &[String]) {
        let mut file = None;
        let mut fit = (3840u32, 2160u32);
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--fit" => {
                    fit = (
                        it.next().and_then(|s| s.parse().ok()).unwrap_or(3840),
                        it.next().and_then(|s| s.parse().ok()).unwrap_or(2160),
                    )
                }
                f => file = Some(f.to_string()),
            }
        }
        let Some(file) = file else {
            eprintln!("hdrprobe: no file given");
            std::process::exit(2);
        };
        let path = Path::new(&file);
        println!("== hdrprobe {file}");
        unsafe {
            if let Err(e) = hdrprobe_inner(path, fit) {
                println!("  HDRPROBE FAILED: {}", hr_name(&e));
            }
        }
    }

    /// MFVideoTransFunc value → a readable name (the ones we care about).
    fn transfer_name(t: u32) -> &'static str {
        match t {
            0 => "unknown",
            1 => "10 (linear)",
            4 => "709",
            5 => "240M",
            6 => "sRGB",
            7 => "28",
            15 => "2020_const",
            16 => "2084/PQ",
            17 => "HLG",
            _ => "other",
        }
    }

    unsafe fn hdrprobe_inner(path: &Path, fit_box: (u32, u32)) -> windows::core::Result<()> {
        use windows::core::Interface;
        use windows::Win32::Media::MediaFoundation::{
            IMF2DBuffer2, MF2DBuffer_LockFlags_Read, MFVideoFormat_P010,
        };
        // Native colorimetry (a plain reader).
        let reader0 = open_reader(path, true)?;
        let info = native_info(&reader0)?;
        drop(reader0);
        let fitted = fit_dims(info.w, info.h, fit_box);
        println!(
            "  native: {} {}x{} {:.3}fps  prim={:?} trans={:?}{} matrix={:?} range={:?}",
            info.codec,
            info.w,
            info.h,
            info.fps,
            info.primaries,
            info.transfer,
            info.transfer
                .map(|t| format!(" [{}]", transfer_name(t)))
                .unwrap_or_default(),
            info.matrix,
            info.range,
        );

        // Hardware reader, P010 output, advanced video processing ON (as the
        // producer would run it).
        let manager = dxgi_manager()?;
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 3)?;
        let attrs = attrs.expect("attrs");
        attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
        attrs.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &manager)?;
        attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        let reader: IMFSourceReader =
            MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attrs)?;
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        reader.SetStreamSelection(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, true)?;

        let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let out = MFCreateMediaType()?;
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_P010)?;
        out.SetUINT64(
            &MF_MT_FRAME_SIZE,
            (((fitted.0 & !1) as u64) << 32) | (fitted.1 & !1) as u64,
        )?;
        match reader.SetCurrentMediaType(video, None, &out) {
            Ok(()) => println!("  P010 negotiation: ACCEPTED"),
            Err(e) => {
                println!("  P010 negotiation: REJECTED {}", hr_name(&e));
                return Ok(());
            }
        }
        let cur = reader.GetCurrentMediaType(video)?;
        let packed = cur.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
        let (ow, oh) = ((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32);
        let otrans = cur.GetUINT32(&MF_MT_TRANSFER_FUNCTION).ok();
        let oprim = cur.GetUINT32(&MF_MT_VIDEO_PRIMARIES).ok();
        let omatrix = cur.GetUINT32(&MF_MT_YUV_MATRIX).ok();
        let orange = cur.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE).ok();
        println!(
            "  P010 output: {ow}x{oh}  prim={oprim:?} trans={otrans:?}{} matrix={omatrix:?} range={orange:?}",
            otrans.map(|t| format!(" [{}]", transfer_name(t))).unwrap_or_default(),
        );
        println!(
            "  >>> {}",
            match otrans {
                Some(16) | Some(17) =>
                    "output KEEPS PQ/HLG → MF passes HDR through raw → shader-EOTF path VALID",
                Some(_) | None =>
                    "output is NOT PQ/HLG → MF converted → shader would double-apply (gate stays)",
            }
        );

        // Read one frame + report a few 16-bit sample codes (10-bit high-aligned).
        let mut flags = 0u32;
        let mut ts = 0i64;
        let mut sample: Option<IMFSample> = None;
        reader.ReadSample(
            video,
            0,
            None,
            Some(&mut flags),
            Some(&mut ts),
            Some(&mut sample),
        )?;
        if let Some(sample) = sample {
            let buffer = sample.GetBufferByIndex(0)?;
            if let Ok(b2d) = buffer.cast::<IMF2DBuffer2>() {
                let mut scan0: *mut u8 = std::ptr::null_mut();
                let mut pitch: i32 = 0;
                let mut startp: *mut u8 = std::ptr::null_mut();
                let mut len: u32 = 0;
                b2d.Lock2DSize(
                    MF2DBuffer_LockFlags_Read,
                    &mut scan0,
                    &mut pitch,
                    &mut startp,
                    &mut len,
                )?;
                let row = std::slice::from_raw_parts(scan0, (ow as usize * 2).min(len as usize));
                let codes: Vec<u16> = row
                    .chunks_exact(2)
                    .take(6)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]) >> 6)
                    .collect();
                println!("  first Y 10-bit codes: {codes:?} (0..=1023)");
                b2d.Unlock2D()?;
            } else {
                println!("  (sample buffer is not IMF2DBuffer2 — no sample dump)");
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------- spike

    fn spike(args: &[String]) {
        let mut file = None;
        let mut fit = (3840u32, 2160u32);
        let mut frames = 90usize;
        let mut dump: Option<std::path::PathBuf> = None;
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--fit" => {
                    fit = (
                        it.next().and_then(|s| s.parse().ok()).unwrap_or(3840),
                        it.next().and_then(|s| s.parse().ok()).unwrap_or(2160),
                    )
                }
                "--frames" => frames = it.next().and_then(|s| s.parse().ok()).unwrap_or(90),
                "--dump" => dump = it.next().map(Into::into),
                f => file = Some(f.to_string()),
            }
        }
        let Some(file) = file else {
            eprintln!("spike: no file given");
            std::process::exit(2);
        };
        let path = Path::new(&file);
        if let Some(d) = &dump {
            let _ = std::fs::create_dir_all(d);
        }
        println!("== {file}");
        unsafe {
            if let Err(e) = spike_inner(path, fit, frames, dump.as_deref()) {
                println!("  SPIKE FAILED: {}", hr_name(&e));
            }
        }
    }

    unsafe fn spike_inner(
        path: &Path,
        fit_box: (u32, u32),
        max_frames: usize,
        dump: Option<&Path>,
    ) -> windows::core::Result<()> {
        // --- open + native info -------------------------------------------------
        let t0 = Instant::now();
        let reader = open_reader(path, true)?;
        let open_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let info = native_info(&reader)?;
        println!(
            "  native: {} {}x{} {:.3}fps dur={:?} rot={:?} audio={} prim={:?} trans={:?} matrix={:?} range={:?} (open={open_ms:.0}ms)",
            info.codec,
            info.w,
            info.h,
            info.fps,
            info.duration,
            info.rotation,
            info.has_audio,
            info.primaries,
            info.transfer,
            info.matrix,
            info.range,
        );

        // --- A: native-size RGB32 (the current Live-Photo path's negotiation) ---
        let (nw, nh, nstride) = negotiate_rgb32(&reader, None)?;
        let t = Instant::now();
        let a = pump(&reader, nw, nh, nstride, max_frames, None)?;
        let a_total = t.elapsed();
        println!(
            "  A native-size RGB32: {nw}x{nh} frames={} total={:.0}ms eos={} type_changes={}",
            a.pts.len(),
            a_total.as_secs_f64() * 1000.0,
            a.eos,
            a.type_changes,
        );
        stats_line("read", &a.read_ms);
        stats_line("copy", &a.copy_ms);

        // --- PTS report from run A ----------------------------------------------
        if a.pts.len() >= 2 {
            let deltas: Vec<i64> = a.pts.windows(2).map(|w| w[1] - w[0]).collect();
            let monotonic = deltas.iter().all(|&d| d > 0);
            let (min_d, max_d) = (
                *deltas.iter().min().unwrap() as f64 / 10_000.0,
                *deltas.iter().max().unwrap() as f64 / 10_000.0,
            );
            println!(
                "    pts: first={:.2}ms monotonic={monotonic} delta_min={min_d:.2}ms delta_max={max_d:.2}ms vfr={}",
                a.pts[0] as f64 / 10_000.0,
                max_d - min_d > 1.0,
            );
        }

        // --- B: fitted RGB32 (MF video processor scales) — fresh reader ---------
        let (fw, fh) = fit_dims(info.w, info.h, fit_box);
        let reader_b = open_reader(path, true)?;
        match negotiate_rgb32(&reader_b, Some((fw, fh))) {
            Ok((bw, bh, bstride)) => {
                let honored = (bw, bh) == (fw, fh);
                let t = Instant::now();
                let dump_png = dump.map(|d| {
                    d.join(format!(
                        "{}_fitted.png",
                        path.file_stem().unwrap_or_default().to_string_lossy()
                    ))
                });
                let b = pump(&reader_b, bw, bh, bstride, max_frames, dump_png.as_deref())?;
                let b_total = t.elapsed();
                println!(
                    "  B fitted RGB32: asked {fw}x{fh} got {bw}x{bh} (honored={honored}) frames={} total={:.0}ms",
                    b.pts.len(),
                    b_total.as_secs_f64() * 1000.0,
                );
                stats_line("read", &b.read_ms);
                stats_line("copy", &b.copy_ms);
            }
            Err(e) => println!("  B fitted RGB32: negotiation REJECTED: {}", hr_name(&e)),
        }

        // --- C: seek to 50% + decode forward ------------------------------------
        if let Some(dur) = info.duration {
            let reader_c = open_reader(path, true)?;
            let (cw, ch, cstride) = negotiate_rgb32(&reader_c, Some((fw, fh)))
                .or_else(|_| negotiate_rgb32(&reader_c, None))?;
            // Warm the pipeline with one frame first (a cold-open seek conflates
            // decoder init with seek cost).
            let _ = pump(&reader_c, cw, ch, cstride, 1, None)?;
            let target_hns = (dur.as_nanos() / 2 / 100) as i64;
            let t = Instant::now();
            let pos = propvariant_i8(target_hns);
            reader_c.SetCurrentPosition(&GUID::zeroed(), &pos)?;
            let set_ms = t.elapsed().as_secs_f64() * 1000.0;
            // Decode forward, discarding frames before the target.
            let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
            let mut discarded = 0usize;
            let mut landed: Option<i64> = None;
            for _ in 0..600 {
                let mut flags = 0u32;
                let mut ts = 0i64;
                let mut sample: Option<IMFSample> = None;
                reader_c.ReadSample(
                    video,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut ts),
                    Some(&mut sample),
                )?;
                if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
                    break;
                }
                if sample.is_none() {
                    continue;
                }
                if ts >= target_hns {
                    landed = Some(ts);
                    break;
                }
                discarded += 1;
            }
            let total_ms = t.elapsed().as_secs_f64() * 1000.0;
            match landed {
                Some(ts) => println!(
                    "  C seek→50% ({:.1}s): SetCurrentPosition={set_ms:.1}ms land={total_ms:.0}ms discarded={discarded} landed_at={:.1}s (err={:+.0}ms)",
                    target_hns as f64 / 10_000_000.0,
                    ts as f64 / 10_000_000.0,
                    (ts - target_hns) as f64 / 10_000.0,
                ),
                None => println!("  C seek: no frame ≥ target within 600 reads (set={set_ms:.1}ms)"),
            }
            // Second seek on the now-quiescent reader (+2 s): is the ~1 s a one-time
            // pipeline flush, or does every scrub step pay it?
            let target2 = target_hns + 20_000_000;
            let t = Instant::now();
            let pos2 = propvariant_i8(target2);
            reader_c.SetCurrentPosition(&GUID::zeroed(), &pos2)?;
            let set2_ms = t.elapsed().as_secs_f64() * 1000.0;
            let mut discarded2 = 0usize;
            let mut landed2 = false;
            for _ in 0..600 {
                let mut flags = 0u32;
                let mut ts = 0i64;
                let mut sample: Option<IMFSample> = None;
                reader_c.ReadSample(
                    video,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut ts),
                    Some(&mut sample),
                )?;
                if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
                    break;
                }
                if sample.is_none() {
                    continue;
                }
                if ts >= target2 {
                    landed2 = true;
                    break;
                }
                discarded2 += 1;
            }
            println!(
                "  C2 second seek (+2s): SetCurrentPosition={set2_ms:.1}ms land={:.0}ms discarded={discarded2} landed={landed2}",
                t.elapsed().as_secs_f64() * 1000.0,
            );
            // --- D: cancellation cost = dropping this mid-stream reader ---------
            let t = Instant::now();
            drop(reader_c);
            println!(
                "  D reader drop (mid-stream): {:.0}ms",
                t.elapsed().as_secs_f64() * 1000.0
            );

            // --- E: fresh reader, position set BEFORE the first ReadSample -------
            // If this is fast, "seek = recreate reader + position + read" beats
            // paying ~1 s of SetCurrentPosition on a warm reader every scrub step.
            let t = Instant::now();
            let reader_e = open_reader(path, true)?;
            let (ew, eh, _es) = negotiate_rgb32(&reader_e, Some((fw, fh)))
                .or_else(|_| negotiate_rgb32(&reader_e, None))?;
            let open2_ms = t.elapsed().as_secs_f64() * 1000.0;
            let t = Instant::now();
            let pos_e = propvariant_i8(target_hns);
            reader_e.SetCurrentPosition(&GUID::zeroed(), &pos_e)?;
            let sete_ms = t.elapsed().as_secs_f64() * 1000.0;
            let mut discarded_e = 0usize;
            let mut landed_e = false;
            for _ in 0..600 {
                let mut flags = 0u32;
                let mut ts = 0i64;
                let mut sample: Option<IMFSample> = None;
                reader_e.ReadSample(
                    video,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut ts),
                    Some(&mut sample),
                )?;
                if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
                    break;
                }
                if sample.is_none() {
                    continue;
                }
                if ts >= target_hns {
                    landed_e = true;
                    break;
                }
                discarded_e += 1;
            }
            println!(
                "  E fresh-reader seek: open+negotiate={open2_ms:.0}ms set={sete_ms:.1}ms land={:.0}ms discarded={discarded_e} landed={landed_e} ({ew}x{eh})",
                t.elapsed().as_secs_f64() * 1000.0,
            );
        } else {
            println!("  C seek: skipped (container reports no duration)");
        }

        Ok(())
    }

    fn fit_dims(w: u32, h: u32, fit: (u32, u32)) -> (u32, u32) {
        if w == 0 || h == 0 {
            return fit;
        }
        let scale = (fit.0 as f64 / w as f64)
            .min(fit.1 as f64 / h as f64)
            .min(1.0);
        // Even dims keep YUV-derived pipelines happy.
        let fw = ((w as f64 * scale).round() as u32).max(2) & !1;
        let fh = ((h as f64 * scale).round() as u32).max(2) & !1;
        (fw, fh)
    }

    // ---------------------------------------------------------------- sweep

    fn sweep(files: &[String]) {
        if files.is_empty() {
            eprintln!("sweep: no files given");
            std::process::exit(2);
        }
        for f in files {
            let path = Path::new(f);
            let verdict = unsafe { sweep_one(path) };
            match verdict {
                Ok(info) => println!(
                    "OK    {f}  [{} {}x{} {:.2}fps audio={} rot={:?} prim={:?} trans={:?} matrix={:?} range={:?}]",
                    info.codec,
                    info.w,
                    info.h,
                    info.fps,
                    info.has_audio,
                    info.rotation,
                    info.primaries,
                    info.transfer,
                    info.matrix,
                    info.range,
                ),
                Err((stage, e)) => println!("FAIL  {f}  [{stage}: {}]", hr_name(&e)),
            }
        }
    }

    /// Open → select video → RGB32 → one frame. The runtime capability probe the
    /// poster path will run; the failing *stage* tells the story (no container
    /// handler vs no codec vs decode failure).
    unsafe fn sweep_one(path: &Path) -> Result<NativeInfo, (&'static str, windows::core::Error)> {
        let reader = open_reader(path, true).map_err(|e| ("open", e))?;
        let info = native_info(&reader).map_err(|e| ("native-type", e))?;
        let (w, h, stride) = negotiate_rgb32(&reader, None).map_err(|e| ("negotiate", e))?;
        let stats = pump(&reader, w, h, stride, 1, None).map_err(|e| ("read", e))?;
        if stats.pts.is_empty() {
            return Err((
                "no-frame",
                windows::core::Error::from_hresult(windows::core::HRESULT(-1)),
            ));
        }
        Ok(info)
    }
}
