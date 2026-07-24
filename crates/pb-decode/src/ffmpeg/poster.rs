//! The FFmpeg video **poster + probe** (plan §8-poster): the clip's first
//! non-black frame as a [`DecodedImage`], and the reader-sourced stream facts
//! behind the inspector's video rows — for the containers/codecs the OS
//! backends can't open (ALL video on Linux; MKV/WebM/VP8/VP9/AV1 on macOS).
//!
//! Mirrors the Windows `mf_poster` policy exactly: the shared mean-luma walk
//! (`poster_frame_bright_enough`, ~1 s / 30 frames, last-sampled-frame
//! fallback), fitted decode, rotation + color identical to playback (the
//! producer shares `probe`/`convert`). Honors the decode pool's `cancel` flag
//! **inside** blocking libav work via the AVIO interrupt callback (plan §6) —
//! task #83's multi-consumer pool makes prompt cancellation matter.

use std::sync::atomic::AtomicBool;

use ffmpeg_next as ff;

use std::time::Duration;

use super::convert::FrameConverter;
use super::io::FfInput;
use super::probe::{fit_dims, video_facts, VideoFacts};
use crate::poster_walk::{walk_poster, Best, PosterBackend, ScanOutcome, SeekError};
use crate::video::{
    poster_judge_at_scale, poster_judge_scrgb, rational_to_hns, VideoInput, POSTER_DEADLINE,
};
use crate::{DecodeError, DecodedImage, FitBox, PixelFormat};

/// Stamped when a stream's time base cannot produce a real timestamp — an
/// unmistakable non-locator, deliberately not `0` (which is a legitimate
/// first-frame timestamp).
const NO_LOCATOR_HNS: i64 = i64::MIN;

/// Decode the clip's first non-black frame (capped walk, last-frame fallback)
/// at `fit`, from a path or in-RAM archive bytes. Read-only, RAM-only.
pub fn ff_decode_video_poster(
    input: &VideoInput,
    fit: Option<FitBox>,
    cancel: &AtomicBool,
) -> Result<DecodedImage, DecodeError> {
    poster_inner(input, fit, cancel).map_err(DecodeError::Corrupt)
}

/// Build the video decoder for `index`, attaching hardware decode
/// (VideoToolbox / VAAPI) when the platform + codec allow — the same accel the
/// playback [`Reader`](super::video_producer) uses. A 4K HEVC/HDR poster walk
/// decodes up to ~30 head frames + 12-frame deep-seek bursts; in **software**
/// that's seconds (the "video takes ~5 s to show a poster" cost). The returned
/// [`HwSession`](super::hw::HwSession) must outlive the decoder — the caller
/// stores it. Falls back to software transparently (a rare HW-open failure
/// retries software; `PB_NO_HWACCEL` forces it).
fn decoder_for(
    ctx: &mut ff::format::context::Input,
    index: usize,
) -> Result<(ff::decoder::Video, Option<super::hw::HwSession>), DecodeError> {
    let corrupt = |e: String| DecodeError::Corrupt(e);
    let params = ctx
        .streams()
        .find(|s| s.index() == index)
        .ok_or_else(|| DecodeError::Corrupt("video stream vanished".into()))?
        .parameters();
    let codec_id = params.id();
    let mut cctx = ff::codec::context::Context::from_parameters(params)
        .map_err(|e| corrupt(format!("FFmpeg decoder: {e}")))?;
    let mut hw = super::hw::try_enable(&mut cctx, codec_id);
    match cctx.decoder().video() {
        Ok(d) => Ok((d, hw)),
        // A rare HW-attached open failure: drop the device and retry pure software
        // (mirrors the playback reader — belt-and-suspenders, not the usual path).
        Err(_) if hw.is_some() => {
            hw = None;
            let params = ctx
                .streams()
                .find(|s| s.index() == index)
                .ok_or_else(|| DecodeError::Corrupt("video stream vanished".into()))?
                .parameters();
            let d = ff::codec::context::Context::from_parameters(params)
                .and_then(|c| c.decoder().video())
                .map_err(|e| corrupt(format!("FFmpeg decoder: {e}")))?;
            Ok((d, hw))
        }
        Err(e) => Err(corrupt(format!("FFmpeg decoder: {e}"))),
    }
}

/// Holds the demux/decode state for one poster attempt so the head walk and each
/// deep-seek burst can share a single open context (the FFmpeg producer seeks
/// **in place** — no reopen, unlike the Windows MF reader).
struct PosterWalk<'a> {
    opened: FfInput<'a>,
    decoder: ff::decoder::Video,
    /// Hardware decode device, kept alive for the decoder's lifetime (`None` in
    /// software). Declared after `decoder` so it drops after it.
    _hw: Option<super::hw::HwSession>,
    /// The **reduced-scale** converter used only to score each candidate's
    /// brightness cheaply; the winner is converted at full fit afterward.
    walk_conv: FrameConverter,
    facts: VideoFacts,
    packet: ff::Packet,
    eof_sent: bool,
    /// Last timestamp handed out, in stream units — the anchor the deterministic
    /// synthesis walks forward when a container carries no PTS.
    last_ts: i64,
}

impl PosterWalk<'_> {
    /// A frame's ABSOLUTE timestamp in 100 ns units — the unit a replay locator
    /// speaks (task #121). Deliberately NOT `VideoFacts::pts_to_duration`: that
    /// clamps non-positive PTS to zero and round-trips through `f64`, which is
    /// fine for a display clock and wrong for an identity locator.
    ///
    /// Fed from `Frame::timestamp()` — FFmpeg's **best-effort** timestamp, the same
    /// accessor the playback producer stamps with (`video_producer.rs:526`). Reading
    /// raw `pts()` instead would synthesize whenever FFmpeg had *derived* a usable
    /// timestamp, so a poster locator could disagree with playback's clock for the
    /// very same frame (Codex #121-3a P1).
    ///
    /// A container carrying no timestamp at all gets the producer's deterministic
    /// synthesis (previous + one frame interval) rather than a silent zero — "0" is
    /// a legitimate timestamp and must never double as "unknown".
    fn stamp_hns(&mut self, pts: Option<i64>) -> i64 {
        let units = pts.unwrap_or_else(|| {
            let step = if self.facts.fps > 0.0 {
                self.facts
                    .duration_to_pts(Duration::from_secs_f64(1.0 / self.facts.fps))
                    .max(1)
            } else {
                1
            };
            self.last_ts + step
        });
        self.last_ts = units;
        let (num, den) = self.facts.time_base;
        // A stream whose time base won't convert has NO usable locator. Falling back
        // to 0 would alias the legitimate "first frame of the clip" timestamp, so use
        // an unmistakable sentinel instead (Codex #121-3a P2): nothing may replay from
        // it, and the #114-parity work must refuse it rather than seek to it.
        rational_to_hns(units, num as i64, den as i64).unwrap_or(NO_LOCATOR_HNS)
    }
}

impl PosterBackend for PosterWalk<'_> {
    type Frame = ff::frame::Video;

    fn duration(&self) -> Option<Duration> {
        self.facts.duration
    }

    fn cancelled(&self) -> bool {
        self.opened.cancelled()
    }

    /// Seek in place to a keyframe at/around `target` and reset decode state.
    /// Mirrors the producer's seek (start-time base, allow landing past the
    /// target on a start-offset edge).
    ///
    /// FFmpeg surfaces no typed "this container refuses positioned reads" class
    /// the way MF's `MF_E_INVALID_POSITION` does, so every failure maps to
    /// [`SeekError::Failed`] — try the next offset — which is exactly today's
    /// behavior.
    fn seek(&mut self, target: Duration) -> Result<(), SeekError> {
        if self.opened.cancelled() {
            return Err(SeekError::Cancelled);
        }
        let base = self.facts.start_time.unwrap_or(0);
        let target_units = base.saturating_add(self.facts.duration_to_pts(target));
        let idx = self.facts.index as i32;
        let mut rc = unsafe {
            let ctx = self.opened.ctx().as_mut_ptr();
            ff::ffi::avformat_seek_file(ctx, idx, i64::MIN, target_units, target_units, 0)
        };
        if rc < 0 {
            // Nothing at/before the target — allow landing past it.
            rc = unsafe {
                let ctx = self.opened.ctx().as_mut_ptr();
                ff::ffi::avformat_seek_file(ctx, idx, i64::MIN, target_units, i64::MAX, 0)
            };
        }
        if rc < 0 {
            // A cancel can land *inside* `avformat_seek_file` (the AVIO interrupt
            // callback aborts it), which surfaces as a plain negative rc. Report the
            // documented class rather than letting it read as "this offset failed".
            if self.opened.cancelled() {
                return Err(SeekError::Cancelled);
            }
            return Err(SeekError::Failed);
        }
        self.decoder.flush();
        self.eof_sent = false;
        Ok(())
    }

    /// Decode up to `limit` frames from the current position, judging each into
    /// `best`.
    fn scan(
        &mut self,
        limit: usize,
        best: &mut Best<ff::frame::Video>,
        deadline: std::time::Instant,
    ) -> Result<ScanOutcome, DecodeError> {
        let mut decoded_count = 0usize;
        while decoded_count < limit {
            if self.opened.cancelled() {
                return Err(DecodeError::Corrupt("poster walk cancelled".into()));
            }
            // The driver only checks BETWEEN bursts, so the per-sample check is this
            // backend's half of the contract (`poster_walk::PosterBackend::scan`):
            // without it a single burst's decode+transfer+convert can overrun the
            // watchdog. Expiry is `Stop` — keep the best so far — never an error.
            if std::time::Instant::now() >= deadline {
                return Ok(ScanOutcome::Stop);
            }
            // Pull every ready frame before feeding more packets.
            let mut decoded = ff::frame::Video::empty();
            while decoded_count < limit && self.decoder.receive_frame(&mut decoded).is_ok() {
                // A hardware decode leaves the frame on the GPU (VideoToolbox /
                // VAAPI surface) — pull it to a CPU NV12/P010 frame before
                // scoring; software frames pass straight through.
                let transferred =
                    super::hw::transfer_if_hw(&decoded).map_err(DecodeError::Corrupt)?;
                let src = transferred.as_ref().unwrap_or(&decoded);
                // Judge at the reduced WALK scale — the full-res convert happens
                // once, on the winner only. `best` holds the raw frame for that.
                let ts_hns = self.stamp_hns(src.timestamp());
                let (pixels, w, _h) = self.walk_conv.convert(src).map_err(DecodeError::Corrupt)?;
                decoded_count += 1;
                // Rank by score (for the fallback); stop only on a *genuinely good*
                // frame — bright AND textured — so a white/vignette title card never
                // ends the walk. HDR (scene-linear fp16) keeps the simple
                // bright/not-bright split (contrast is hard to judge pre-tone-map).
                // Both rules are the SHARED ones in `video.rs` (task #121); the
                // walk converter already outputs judge-sized pixels, so the
                // at-scale entry skips a per-candidate buffer clone.
                let (good, score) = if self.walk_conv.output_format() == PixelFormat::Rgba16F {
                    poster_judge_scrgb(&pixels)
                } else {
                    poster_judge_at_scale(&pixels, w)
                };
                // A good frame is the winner outright; return it, not whatever
                // out-ranked it earlier. Otherwise rank it for the fallback —
                // the refcounted clone happens ONLY on replacement.
                if good {
                    best.win(src.clone(), ts_hns);
                    return Ok(ScanOutcome::Good);
                }
                best.consider_with(score, ts_hns, || src.clone());
            }
            if self.eof_sent {
                break; // decoder fully drained
            }
            match super::read_into_reused(&mut self.packet, self.opened.ctx()) {
                Ok(()) => {
                    if self.packet.stream() == self.facts.index {
                        // A corrupt packet is skipped, not fatal.
                        let _ = self.decoder.send_packet(&self.packet);
                    }
                }
                Err(ff::Error::Eof) => {
                    let _ = self.decoder.send_eof();
                    self.eof_sent = true;
                }
                Err(ff::Error::Other { errno }) if errno == ff::util::error::EAGAIN => {}
                Err(e) => return Err(DecodeError::Corrupt(format!("FFmpeg read: {e}"))),
            }
        }
        Ok(ScanOutcome::Exhausted)
    }
}

fn poster_inner(
    input: &VideoInput,
    fit: Option<FitBox>,
    cancel: &AtomicBool,
) -> Result<DecodedImage, String> {
    let mut opened = FfInput::open(input, Some(cancel))?;
    opened.set_op_deadline(Some(POSTER_DEADLINE));
    // The driver's between-burst deadline, armed at the same instant as the AVIO
    // interrupt deadline above so the two never disagree about when the walk is out
    // of time.
    let deadline = std::time::Instant::now() + POSTER_DEADLINE;
    let facts = video_facts(opened.ctx())?;
    let (decoder, hw) = decoder_for(opened.ctx(), facts.index).map_err(|e| e.to_string())?;

    // Output geometry: fit the SAR-corrected display dims, then map back to
    // pre-rotation axes for the scaler (the converter rotates after).
    let (disp_w, disp_h) = facts.display_dims();
    let (fw, fh) = match fit {
        Some(f) => fit_dims(disp_w, disp_h, f),
        None => (disp_w, disp_h),
    };
    let pre_rot = |w: u32, h: u32| {
        if facts.rotation % 180 == 90 {
            (h, w)
        } else {
            (w, h)
        }
    };
    let mk_conv = |out: (u32, u32)| {
        FrameConverter::new(
            (facts.width, facts.height),
            out,
            facts.rotation,
            &decoder,
            false, // posters are stills → RGBA, never the planar GPU path
            false,
        )
    };
    // Two converters: a reduced-scale one to *score* every candidate cheaply, and
    // the full-fit one that converts the winner exactly once. Scoring at 4K was the
    // dominant poster cost (measured ~4× the small-scale walk).
    let (ww, wh) = walk_dims(fw, fh);
    let walk_conv = mk_conv(pre_rot(ww, wh));
    let full_conv = mk_conv(pre_rot(fw, fh));

    let mut walk = PosterWalk {
        opened,
        decoder,
        _hw: hw,
        walk_conv,
        facts,
        packet: ff::Packet::empty(),
        eof_sent: false,
        last_ts: 0,
    };
    let mut best = Best::new();

    // The head walk, then — only if it found nothing good — the shallow→deep seeks
    // past the intro. The policy itself is SHARED (task #121,
    // `poster_walk::walk_poster`): this backend supplies only the mechanics. Both
    // outcomes assemble from `best` (the fallback is the least-black frame).
    let _good = walk_poster(&mut walk, &mut best, deadline).map_err(|e| match e {
        DecodeError::Corrupt(m) => m,
        DecodeError::Unsupported => "unsupported".to_string(),
    })?;

    assemble_poster(full_conv, best, walk.facts.codec, disp_w, disp_h)
}

/// The reduced scale to *score* candidates at: the shared judge WIDTH (task
/// #114, `video::POSTER_JUDGE_WIDTH`) so the MF and FFmpeg walks pick the SAME
/// frame for the same clip — the judge is the resolution-independence contract,
/// not a private optimization. Width-anchored like `poster_judge_frame`
/// (phase-1 review f7: a long-edge reduction judged portrait clips at a
/// different scale than MF, making the detail threshold backend-dependent).
/// Small clips (test fixtures) walk at their native size unchanged.
fn walk_dims(fw: u32, fh: u32) -> (u32, u32) {
    let walk_max = crate::video::POSTER_JUDGE_WIDTH;
    if fw <= walk_max {
        return (fw, fh);
    }
    let s = walk_max as f32 / fw as f32;
    (walk_max & !1, ((fh as f32 * s).round() as u32).max(2) & !1)
}

/// Assemble the winning **raw** frame into a `DecodedImage` (poster ≡ HDR-still
/// shape for the renderer), converting it once at the full display fit. `Err`
/// only when no frame decoded at all.
fn assemble_poster(
    mut full_conv: FrameConverter,
    best: Best<ff::frame::Video>,
    codec: &'static str,
    disp_w: u32,
    disp_h: u32,
) -> Result<DecodedImage, String> {
    let (winner, _ts_hns) = best.take().ok_or("video decoded no frames")?;
    let (pixels, w, h) = full_conv.convert(&winner)?;
    let sc = full_conv.source_color();
    // HDR (PQ/HLG) posters leave as fp16 scene-linear scRGB — the exact shape
    // `common::finalize_hdr_scrgb` gives HDR stills, so the renderer treats a
    // video poster and an HDR photo identically (plan §9).
    let hdr = full_conv.output_format() == PixelFormat::Rgba16F;
    Ok(DecodedImage {
        width: w,
        height: h,
        orig_width: disp_w,
        orig_height: disp_h,
        codec,
        format: full_conv.output_format(),
        pixels,
        is_preview: false,
        color: if hdr {
            crate::ColorTransform::srgb() // already scene-linear; passthrough
        } else {
            super::color::sdr_transform(&sc)
        },
        peak: full_conv.peak(),
        animated: None,
        recovered: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/video")
            .join(name)
    }

    fn poster(name: &str, fit: Option<FitBox>) -> Result<DecodedImage, DecodeError> {
        ff_decode_video_poster(
            &VideoInput::Path(fixture(name)),
            fit,
            &AtomicBool::new(false),
        )
    }

    /// The fixture opens on black frames; the walk must skip them and land on
    /// the color section — the exact contract the Windows poster test locks.
    #[test]
    fn poster_walks_past_the_black_lead() {
        let img = poster("black_then_color.mp4", None).expect("poster");
        assert!(img.is_well_formed());
        assert_eq!((img.width, img.height), (64, 64));
        assert_eq!(img.codec, "H.264");
        assert!(
            crate::video::mean_luma_rgba8(&img.pixels, 1) > crate::video::POSTER_LUMA_MIN,
            "poster landed on a black frame"
        );
    }

    #[test]
    fn poster_fits_and_keeps_even_dims() {
        let img = poster(
            "black_then_color.mp4",
            Some(FitBox {
                max_width: 32,
                max_height: 32,
            }),
        )
        .expect("poster");
        assert_eq!((img.width, img.height), (32, 32));
        assert_eq!((img.orig_width, img.orig_height), (64, 64));
        assert!(img.is_well_formed());
    }

    #[test]
    fn poster_from_in_ram_bytes_matches_the_path_poster() {
        let data = std::sync::Arc::new(std::fs::read(fixture("black_then_color.mp4")).unwrap());
        let by_bytes = ff_decode_video_poster(
            &VideoInput::Bytes {
                data,
                name: "clip.mp4".into(),
            },
            None,
            &AtomicBool::new(false),
        )
        .expect("bytes poster");
        let by_path = poster("black_then_color.mp4", None).expect("path poster");
        assert_eq!(by_bytes.pixels, by_path.pixels, "bytes ≡ path, bit-exact");
    }

    #[test]
    fn cancel_aborts_the_poster() {
        let r = ff_decode_video_poster(
            &VideoInput::Path(fixture("black_then_color.mp4")),
            None,
            &AtomicBool::new(true),
        );
        assert!(r.is_err(), "a cancelled poster must not decode");
    }

    #[test]
    fn probe_reports_the_stream_facts() {
        let info = super::super::details::ff_probe_video_input(&VideoInput::Path(fixture(
            "color_with_tone.mp4",
        )))
        .expect("probe");
        assert_eq!(info.codec, "H.264");
        assert!(info.has_audio);
        assert!(info.duration.is_some());
        assert_eq!(
            info.display_dims(),
            (info.width, info.height),
            "no rotation"
        );
    }

    /// HDR posters mirror HDR stills: fp16 scene-linear, passthrough color,
    /// a real peak — the same `DecodedImage` shape `finalize_hdr_scrgb` makes.
    #[test]
    fn hdr_poster_is_fp16_scene_linear() {
        let img = poster("hdr_pq.mp4", None).expect("hdr poster");
        assert_eq!(img.format, PixelFormat::Rgba16F);
        assert!(img.is_well_formed());
        assert!(!img.color.enabled, "scene-linear → passthrough");
        assert!(img.peak >= 1.0);
    }

    #[test]
    fn garbage_bytes_fail_cleanly() {
        let r = ff_decode_video_poster(
            &VideoInput::Bytes {
                data: std::sync::Arc::new(vec![0xAAu8; 2048]),
                name: "junk.webm".into(),
            },
            None,
            &AtomicBool::new(false),
        );
        assert!(r.is_err());
    }

    /// Poster-generation wall time on a real 4K HEVC/HDR clip — the "why does the
    /// initial video take ~5 s to show a poster" measurement. Fit to a big display
    /// box (like the owner's ultrawide). Run:
    /// `PB_NET_TEST_MKV=/path cargo test -p pb-decode --release --features ffvideo \
    ///   poster_generation_time -- --nocapture --ignored`
    #[test]
    #[ignore = "needs PB_NET_TEST_MKV pointing at a real 4K HEVC clip"]
    fn poster_generation_time() {
        let Ok(path) = std::env::var("PB_NET_TEST_MKV") else {
            eprintln!("skipping: set PB_NET_TEST_MKV");
            return;
        };
        for fit in [
            FitBox {
                max_width: 7680,
                max_height: 2160,
            },
            FitBox {
                max_width: 480,
                max_height: 270,
            },
        ] {
            let t = std::time::Instant::now();
            let img = ff_decode_video_poster(
                &VideoInput::Path(PathBuf::from(&path)),
                Some(fit),
                &AtomicBool::new(false),
            )
            .expect("poster");
            // Report the JUDGE's verdict on the result, not just the timing: the
            // failure this walk exists to prevent is a *black* poster on a film that
            // opens on a studio logo, and a fast black poster is still a bug. SDR
            // only — the scRGB judge reads fp16 and would misread these bytes.
            let verdict = if img.format == PixelFormat::Rgba8 {
                let (good, score) = crate::video::poster_judge(&img.pixels, img.width, img.height);
                format!(
                    " | {} (score {score:.3})",
                    if good { "SCENE" } else { "⚠ weak/dark" }
                )
            } else {
                String::new()
            };
            eprintln!(
                "[poster] fit={}x{} → {}x{} {:?} in {:.2}s{verdict}",
                fit.max_width,
                fit.max_height,
                img.width,
                img.height,
                img.format,
                t.elapsed().as_secs_f64()
            );
        }
    }
}
