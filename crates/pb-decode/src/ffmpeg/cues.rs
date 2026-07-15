//! **Embedded subtitle streams → timed text** (task #90.2).
//!
//! The sidecar tier reads a file beside the video and parses it in pure Rust. This is the
//! other half: the cues *inside* the container — MKV `subrip`/`ass`/`webvtt`, MP4
//! `mov_text` — which need a demuxer to reach and a decoder to unwrap.
//!
//! ## Why this lives at the `ffprobe` level, not next to [`super::demux`]
//!
//! [`super::demux::VideoDemuxer`] is the obvious neighbour and the wrong home: it is
//! gated on `ffvideo`, which **Windows does not build** (MF owns decode there; FFmpeg is
//! present only to read stream tables). Subtitles must work on all three platforms, so
//! this sits with [`super::details`] / [`super::tracks`] under the always-on `ffmpeg`
//! module set, and needs no video decoder — reading a subtitle stream is a demux plus a
//! *subtitle* decoder, both of which the trimmed +3.06 MB Windows FFmpeg enables on
//! purpose (`--enable-decoder=subrip,ass,ssa,movtext,webvtt,text`).
//!
//! That asymmetry is worth knowing: Windows can read cues *in a container* but cannot
//! open a standalone `.srt` at all (no srt/webvtt/ass **demuxer**). It costs us nothing —
//! sidecars are parsed in Rust (`pb_app_core::cues`) and never go near FFmpeg.
//!
//! ## Cost — and why it streams
//!
//! This is a **full linear pass over the container**. Subtitle blocks are scattered
//! through every cluster, so `av_read_frame` has to walk every interleaved packet to find
//! them: reading the cues of a 2-hour film means reading the film. Measured on the corpus
//! MKV (4.4 GB, over SMB): **39 seconds**. Waiting for that before showing the first cue
//! would be indistinguishable from broken.
//!
//! The way out is a ratio, not an optimization. The read runs at I/O speed (~113 MB/s
//! here) and walks the file in **presentation order**; playback consumes the same file at
//! ~1× real time (~1.6 MB/s). The reader is roughly **70× faster than playback needs**,
//! so it only has to hand cues over *as it finds them* and it will stay far ahead of the
//! playhead forever. First cue on screen: well under a second. Hence
//! [`ff_stream_subtitle_cues`], with [`ff_read_subtitle_cues`] kept as the collect-it-all
//! convenience for tests and tools.
//!
//! The one honest hole: **seeking past the read frontier** early on. Jump to 40 minutes
//! two seconds after opening and there are no cues there yet — they arrive as the reader
//! reaches them, and once the pass completes (≤39 s, worst case, on the worst file we
//! have) it is moot forever. Strictly better than the alternative, which has no cues
//! anywhere for the same 39 seconds.
//!
//! **The real fix, later:** the playback demuxer already reads every one of these packets
//! and throws the subtitle ones away. Forwarding them instead would make cues cost zero
//! extra I/O. It is not the v1 answer because it only exists on the routes that use *our*
//! demuxer — and the hard-won rule from this task is that a feature must never be gated
//! on a backend (see the `.taskmaster` post-mortem). This reader works on every route;
//! a ride-along would be an optimization layered under it, never a replacement.
//!
//! It only ever runs on a worker, only when subtitles are on and a video is playing, and
//! it is **cancellable** — navigating away aborts inside libav via the interrupt callback
//! rather than leaving a thread chewing through 20 GB.
//!
//! Read-only and RAM-only: the no-trace guarantee (privacy task #2) holds here.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use ffmpeg_next as ff;

use super::io::FfInput;
use crate::text_cue::{ass_event_text, TextCue};
use crate::video::VideoInput;
use crate::DecodeError;

/// A ceiling on how many cues one stream may produce.
///
/// A film has ~2,000. This is not a performance tuning knob — it is the bound that keeps
/// a hostile or pathological container (a fuzzed stream that decodes to a cue per packet
/// forever) from turning a background read into unbounded RAM growth. Hitting it returns
/// the cues found so far: a truncated track still shows the first two hours of dialogue,
/// which beats showing nothing.
const MAX_CUES: usize = 50_000;

/// Matches [`super::demux`]'s read budget. A stalled network read must not pin the worker.
const READ_DEADLINE: Duration = Duration::from_secs(10);

/// Hand a batch over at least this often, so the first cue reaches the screen promptly
/// rather than waiting for [`BATCH_CUES`] to fill. Films open on silence.
const FLUSH_EVERY: Duration = Duration::from_millis(100);

/// …and at most this many cues per batch, so a dense stream doesn't send one message per
/// cue. Purely to keep the channel quiet; correctness does not depend on it.
const BATCH_CUES: usize = 32;

/// Every text cue on subtitle stream `stream_index`, collected.
///
/// Convenience over [`ff_stream_subtitle_cues`] for tests and tools. **The app does not
/// use this** — it streams, because on a large file this does not return for a long time.
/// See the module docs.
pub fn ff_read_subtitle_cues(
    input: &VideoInput,
    stream_index: usize,
    cancel: Arc<AtomicBool>,
) -> Result<Vec<TextCue>, DecodeError> {
    let mut all = Vec::new();
    ff_stream_subtitle_cues(input, stream_index, cancel, |batch| {
        all.extend(batch);
        true
    })?;
    Ok(all)
}

/// Read every text cue on subtitle stream `stream_index`, handing them to `on_batch` in
/// presentation order **as they are found**.
///
/// `stream_index` is the container's real stream index — exactly what
/// [`crate::tracks::TrackLocator::FfStream`] carries, which is what
/// [`super::tracks::catalog_from_input`] recorded when it built the catalog. So the
/// caller never guesses an index; it hands back the one the catalog minted.
///
/// `on_batch` returns `false` to stop the read (the consumer went away). Flipping
/// `cancel` also aborts it promptly — the interrupt callback fires inside libav's
/// blocking I/O, so it works even mid-read on a slow network file. Both are normal
/// endings, not errors.
///
/// **Off-thread only.** See the module docs on cost.
pub fn ff_stream_subtitle_cues(
    input: &VideoInput,
    stream_index: usize,
    cancel: Arc<AtomicBool>,
    mut on_batch: impl FnMut(Vec<TextCue>) -> bool,
) -> Result<(), DecodeError> {
    super::init::ff_init();
    let mut opened = FfInput::open(input, None).map_err(DecodeError::Corrupt)?;
    opened.set_cancel(cancel);

    // Read the stream's facts, then drop the borrow before touching the context again.
    let (time_base, params) = {
        let ctx = opened.ctx();
        let stream = ctx
            .stream(stream_index)
            .ok_or_else(|| DecodeError::Corrupt(format!("no stream {stream_index}")))?;
        let params = stream.parameters();
        if params.medium() != ff::media::Type::Subtitle {
            return Err(DecodeError::Corrupt(format!(
                "stream {stream_index} is not a subtitle stream"
            )));
        }
        (stream.time_base(), params)
    };

    let mut cctx = ff::codec::context::Context::from_parameters(params)
        .map_err(|e| DecodeError::Corrupt(format!("subtitle codec params: {e}")))?;
    // Some text decoders (webvtt, mov_text) rescale against `pkt_timebase`, and nothing
    // else sets it — `from_parameters` copies the parameters, not the stream's timing.
    // We compute our own timestamps below regardless, but a decoder reading a zeroed
    // timebase is a landmine left armed for no reason.
    unsafe {
        (*cctx.as_mut_ptr()).pkt_timebase = time_base.into();
    }
    let mut decoder = cctx
        .decoder()
        .subtitle()
        .map_err(|e| DecodeError::Corrupt(format!("no subtitle decoder: {e}")))?;

    let mut packet = ff::Packet::empty();
    let mut batch: Vec<TextCue> = Vec::new();
    let mut sent = 0usize;
    let mut last_flush = std::time::Instant::now();

    loop {
        opened.set_op_deadline(Some(READ_DEADLINE));
        let r = packet.read(opened.ctx());
        opened.set_op_deadline(None);
        match r {
            Ok(()) => {}
            Err(ff::Error::Eof) => break,
            Err(ff::Error::Other { errno }) if errno == ff::util::error::EAGAIN => {
                if opened.cancelled() {
                    return Err(DecodeError::Corrupt("cancelled".into()));
                }
                continue;
            }
            Err(e) => return Err(DecodeError::Corrupt(format!("subtitle demux read: {e}"))),
        }
        if packet.stream() != stream_index {
            // Video / audio: the pass has to walk them, not keep them. This branch is
            // where ~99.9% of the file goes, and it is the whole cost of the read.
            continue;
        }

        let mut sub = ff::Subtitle::new();
        match decoder.decode(&packet, &mut sub) {
            Ok(true) => {}
            // No cue in this packet is normal (a decoder can buffer), not an error.
            Ok(false) => continue,
            // ONE bad packet must not cost the whole track. Subtitle streams are hand-made
            // and a single malformed cue in a 2,000-cue film is a normal Tuesday; refusing
            // the file over it would be the wrong trade every time.
            Err(_) => continue,
        }

        // ⚠ `ffmpeg-next` has NO `Drop` for `Subtitle` and never calls `avsubtitle_free`
        // (verified in 8.1.0). The decoder heap-allocates the rects on every successful
        // decode, so without the free below this leaks per cue, forever. Everything read
        // out of `sub` is copied to an owned `String` first.
        let cues = collect_cues(&sub, &packet, time_base);
        unsafe {
            ff::ffi::avsubtitle_free(sub.as_mut_ptr());
        }

        batch.extend(cues);
        if batch.len() >= BATCH_CUES || last_flush.elapsed() >= FLUSH_EVERY {
            sent += batch.len();
            if !on_batch(std::mem::take(&mut batch)) {
                return Ok(()); // the consumer is gone; a normal ending
            }
            last_flush = std::time::Instant::now();
            if sent >= MAX_CUES {
                return Ok(());
            }
        }
    }

    if !batch.is_empty() {
        on_batch(batch);
    }
    Ok(())
}

/// One decoded `AVSubtitle` → the cues it carries. Split out so the timing rules below
/// read as rules rather than as loop body.
fn collect_cues(sub: &ff::Subtitle, packet: &ff::Packet, time_base: ff::Rational) -> Vec<TextCue> {
    // The packet's timestamp is the anchor, not `sub.pts()`: the latter is only rescaled
    // when `pkt_timebase` was set, which nothing guarantees for a context we did not open
    // ourselves. `dts` is the fallback — a subtitle stream has no reordering, so when a
    // container omits pts they are the same number.
    let anchor = packet.pts().or_else(|| packet.dts()).unwrap_or(0);
    let base = ts_to_duration(anchor, time_base);

    // `start_display_time` / `end_display_time` are millisecond offsets from the anchor.
    let start = base + Duration::from_millis(u64::from(sub.start()));
    let end = if sub.end() > 0 {
        base + Duration::from_millis(u64::from(sub.end()))
    } else if packet.duration() > 0 {
        // MKV carries the duration on the packet and leaves end_display_time at 0.
        base + ts_to_duration(packet.duration(), time_base)
    } else {
        // Neither is known. Deliberately degenerate: `CueTrack::from_cues` repairs an
        // end <= start from the *next* cue's start (bounded by FALLBACK_CUE), which is a
        // better answer than any constant we could invent here — and it is the same
        // repair a typo'd sidecar gets.
        start
    };

    sub.rects()
        .filter_map(|rect| {
            let text = match rect {
                // Every FFmpeg text decoder emits ASS. See `ass_event_text`.
                ff::codec::subtitle::Rect::Ass(a) => {
                    ass_event_text(&unsafe { read_c_str((*a.as_ptr()).ass) }?)
                }
                // Rare, but real: a decoder that emits plain text carries no envelope.
                ff::codec::subtitle::Rect::Text(t) => unsafe { read_c_str((*t.as_ptr()).text) }?,
                // Bitmap (PGS/VobSub) is an explicit #90 non-goal — a different pipeline
                // entirely. The catalog already marks these `TrackCapability::Bitmap` and
                // the picker won't offer them, so reaching here means a stale selection.
                _ => return None,
            };
            (!text.trim().is_empty()).then_some(TextCue { start, end, text })
        })
        .collect()
}

/// A C string from FFmpeg → an owned `String`, **lossily**.
///
/// Deliberately not `ffmpeg-next`'s `Text::get()` / `Ass::get()`: those are
/// `from_utf8_unchecked`, which is UB on the non-UTF-8 payloads that genuinely exist in
/// the wild (a CP1252 `.srt` muxed into an MKV without `sub_charenc`). We parse hostile
/// bytes for a living; mojibake on one line is a fine outcome, undefined behaviour is not.
///
/// # Safety
/// `ptr` must be null or a valid NUL-terminated C string owned by the subtitle rect.
unsafe fn read_c_str(ptr: *mut std::os::raw::c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    Some(String::from_utf8_lossy(std::ffi::CStr::from_ptr(ptr).to_bytes()).into_owned())
}

/// A timestamp in `time_base` units → wall time. Saturating at zero: a negative start
/// (an edit-list offset the container applied) means "already on screen", not "before the
/// file began".
fn ts_to_duration(ts: i64, time_base: ff::Rational) -> Duration {
    let (num, den) = (
        i64::from(time_base.numerator()),
        i64::from(time_base.denominator()),
    );
    if den == 0 || ts <= 0 {
        return Duration::ZERO;
    }
    let secs = ts as f64 * num as f64 / den as f64;
    Duration::try_from_secs_f64(secs.max(0.0)).unwrap_or(Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timestamp_converts_through_its_time_base() {
        // 1/1000 (MKV's usual): 1500 ticks = 1.5 s.
        let tb = ff::Rational::new(1, 1000);
        assert_eq!(ts_to_duration(1500, tb), Duration::from_millis(1500));
        // 1/90000 (MPEG-TS): 90000 ticks = 1 s.
        assert_eq!(
            ts_to_duration(90_000, ff::Rational::new(1, 90_000)),
            Duration::from_secs(1)
        );
    }

    /// A negative timestamp is an edit-list artifact, not a reason to panic or to wrap
    /// around into a 500-year cue.
    #[test]
    fn a_negative_or_degenerate_timestamp_is_zero_not_a_panic() {
        assert_eq!(
            ts_to_duration(-500, ff::Rational::new(1, 1000)),
            Duration::ZERO
        );
        assert_eq!(
            ts_to_duration(1000, ff::Rational::new(1, 0)),
            Duration::ZERO
        );
    }

    #[test]
    fn a_null_c_string_is_none_not_a_crash() {
        assert_eq!(unsafe { read_c_str(std::ptr::null_mut()) }, None);
    }

    /// The `from_utf8_unchecked` this module exists to avoid. A CP1252 byte in a muxed
    /// subtitle must produce a replacement char, not UB.
    #[test]
    fn invalid_utf8_is_lossy_not_undefined() {
        let raw = std::ffi::CString::new(vec![b'c', b'a', b'f', 0xE9, b'!']).unwrap();
        let got = unsafe { read_c_str(raw.as_ptr() as *mut std::os::raw::c_char) }.unwrap();
        assert_eq!(got, "caf\u{fffd}!");
    }

    /// The whole reader against a **real container**, because everything above is a unit
    /// test of a part and this is the only thing that proves the parts join.
    ///
    /// Gated on a corpus file (the `PB_LIVE_TEST_MOV` pattern) — CI has no films:
    /// ```sh
    /// PB_TEST_SUB_MKV=/path/to/Movie.mkv PB_TEST_SUB_STREAM=2 \
    ///   cargo test -p pb-decode --features ffvideo -- --ignored --nocapture embedded
    /// ```
    /// `--ignored` as well as env-gated: this reads the entire file, which is tens of
    /// seconds on a real film and has no business in a default `cargo test`.
    #[test]
    #[ignore = "needs a real container: set PB_TEST_SUB_MKV + PB_TEST_SUB_STREAM"]
    fn embedded_cues_read_from_a_real_container() {
        let (Ok(path), Ok(idx)) = (
            std::env::var("PB_TEST_SUB_MKV"),
            std::env::var("PB_TEST_SUB_STREAM"),
        ) else {
            eprintln!("PB_TEST_SUB_MKV / PB_TEST_SUB_STREAM not set — skipping");
            return;
        };
        let idx: usize = idx
            .parse()
            .expect("PB_TEST_SUB_STREAM must be a stream index");
        let input = VideoInput::Path(path.into());
        let cancel = Arc::new(AtomicBool::new(false));

        // Streaming is the contract, so assert the streaming property rather than just
        // the total: cues must arrive in more than one batch, and the first must arrive
        // long before the last. That is the only reason subtitles appear promptly.
        let start = std::time::Instant::now();
        let mut batches = 0usize;
        let mut first_batch_at = None;
        let mut cues = Vec::new();
        ff_stream_subtitle_cues(&input, idx, cancel, |b| {
            batches += 1;
            first_batch_at.get_or_insert_with(|| start.elapsed());
            cues.extend(b);
            true
        })
        .expect("read the subtitle stream");

        assert!(!cues.is_empty(), "a real subtitle stream has cues");
        assert!(batches > 1, "cues must STREAM, not arrive in one lump");
        eprintln!(
            "{} cues in {} batches; first batch after {:?}, all of it in {:?}",
            cues.len(),
            batches,
            first_batch_at.unwrap(),
            start.elapsed()
        );

        // Presentation order — what `active_at` scheduling depends on.
        assert!(
            cues.windows(2).all(|w| w[0].start <= w[1].start),
            "cues must arrive in presentation order"
        );
        // Every cue is a real window with real text.
        for c in &cues {
            assert!(!c.text.trim().is_empty(), "no empty cue: {c:?}");
        }
    }

    /// Cancellation is what keeps a nav from leaving a worker walking 20 GB. Returning
    /// `false` from the sink must stop the read promptly, not merely be ignored.
    #[test]
    #[ignore = "needs a real container: set PB_TEST_SUB_MKV + PB_TEST_SUB_STREAM"]
    fn a_sink_that_says_stop_stops_the_read() {
        let (Ok(path), Ok(idx)) = (
            std::env::var("PB_TEST_SUB_MKV"),
            std::env::var("PB_TEST_SUB_STREAM"),
        ) else {
            return;
        };
        let idx: usize = idx.parse().unwrap();
        let input = VideoInput::Path(path.into());
        let start = std::time::Instant::now();
        let mut batches = 0;
        ff_stream_subtitle_cues(&input, idx, Arc::new(AtomicBool::new(false)), |_| {
            batches += 1;
            false // stop after the first
        })
        .expect("a stopped read is a normal ending, not an error");
        assert_eq!(batches, 1, "the read must stop when the sink says stop");
        eprintln!("stopped after {:?}", start.elapsed());
    }
}
