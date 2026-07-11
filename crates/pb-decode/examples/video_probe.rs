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
            _ => {
                eprintln!(
                    "usage: video_probe spike <file> [--fit W H] [--frames N] [--dump <dir>]"
                );
                eprintln!("       video_probe sweep <file>...");
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
