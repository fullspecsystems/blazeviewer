//! Demux-only compressed packet source (video-overhaul Phase 3, plan §3A).
//!
//! The macOS `AVSampleBufferDisplayLayer` presenter wants *compressed* access
//! units, not decoded pixels: FFmpeg demuxes the container (MKV/WebM/… that
//! AVFoundation can't open) and Swift wraps each packet into a `CMSampleBuffer`,
//! handing decode to VideoToolbox. This module is that seam — a sibling of
//! [`super::video_producer`] that reuses [`FfInput`] + [`video_facts`] but
//! **never constructs a decoder**: it reads packets and hands out their raw
//! compressed payload plus the timing/keyframe flags the sample buffer needs.
//!
//! What crosses to Swift:
//! - once, at open: the codec, the coded dimensions, the codec-private data
//!   (the `hvcC`/`avcC` atom → `CMVideoFormatDescription`), the NAL length-prefix
//!   size, and — when present — the Dolby Vision configuration record (so the
//!   `dvcC`/`dvvC` box rides on the format description and VideoToolbox engages
//!   the DoVi path). See [`DemuxStreamInfo`].
//! - per packet: the container-native bytes, PTS/DTS in stream time-base units,
//!   duration, and the sync-sample (keyframe) flag. See [`DemuxPacket`].
//!
//! Reads only, RAM-only: the no-trace guarantee holds here exactly as it does
//! for the decode producer. Cancellation rides the shared [`FfInput`] interrupt
//! flag (plan 1F), so a stuck network read aborts on stop/teardown.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use ffmpeg_next as ff;

use super::io::FfInput;
use super::probe::{video_facts, VideoFacts};
use crate::video::VideoInput;

/// Watchdog budget for one packet read (local/RAM resolve in ms; only hostile
/// or stalled-network input hits this). Mirrors the producer's `OP_DEADLINE`.
const READ_DEADLINE: Duration = Duration::from_secs(10);

/// Which codec the compressed stream carries — decides how Swift builds the
/// `CMVideoFormatDescription` (HEVC vs H.264 parameter sets). `Other` streams
/// route to the FFmpeg Session fallback (this presenter only covers the codecs
/// VideoToolbox decodes from a sample buffer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
    Other,
}

/// A Dolby Vision decoder configuration record (`AV_PKT_DATA_DOVI_CONF`), parsed
/// from the stream side data. The corpus case is **profile 8.1** (single-layer
/// BL+RPU, HDR10-compatible base) — the profile Apple's stack presents as
/// `dvhe.08`. [`Self::box_payload`] is the ISO-BMFF `dvcC`/`dvvC` configuration
/// box Swift attaches to the sample description so VideoToolbox sees the DoVi
/// metadata; without it the same stream decodes as its plain HDR10 base layer.
#[derive(Debug, Clone)]
pub struct DoviConfig {
    pub version_major: u8,
    pub version_minor: u8,
    pub profile: u8,
    pub level: u8,
    pub rpu_present: bool,
    pub el_present: bool,
    pub bl_present: bool,
    pub bl_signal_compatibility_id: u8,
    /// The configuration box four-CC: `dvvC` for profile > 7 (the 8.x/9.x
    /// records that carry `bl_signal_compatibility_id`), else the legacy `dvcC`.
    /// Matches FFmpeg movenc's `mov_write_dvcc_dvvc_tag`.
    pub atom: [u8; 4],
    /// The 24-byte `DOVIDecoderConfigurationRecord` payload (bit-packed, the box
    /// contents after the size+type header).
    pub box_payload: [u8; 24],
}

/// The one-time facts a `CMVideoFormatDescription` needs, beyond [`VideoFacts`].
#[derive(Debug, Clone)]
pub struct DemuxStreamInfo {
    pub facts: VideoFacts,
    pub codec: VideoCodec,
    /// Codec private data: the `hvcC`/`avcC` atom for MP4/MKV (length-prefixed),
    /// or in-band Annex B parameter sets for MPEG-TS. Empty when the container
    /// carries none.
    pub extradata: Vec<u8>,
    /// NAL length-prefix size (1/2/4) read from the `hvcC`/`avcC` atom; `0` when
    /// the stream is Annex B (start-code delimited) rather than length-prefixed.
    pub nal_length_size: u8,
    /// Whether packets are length-prefixed (MP4/MKV) — the representation the
    /// AVSampleBuffer path this module targets consumes directly. Annex-B-only
    /// streams (no usable extradata atom) route to the Session fallback rather
    /// than needing an in-Rust bitstream filter.
    pub length_prefixed: bool,
    pub dovi: Option<DoviConfig>,
}

/// One compressed access unit, container-native (length-prefixed for MP4/MKV).
#[derive(Debug, Clone)]
pub struct DemuxPacket {
    pub data: Vec<u8>,
    /// Presentation / decode timestamps in [`VideoFacts::time_base`] units;
    /// `None` when the container carries none (the presenter synthesizes from
    /// `duration`/fps, as the decode producer's `stamp` does).
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    /// Packet duration in time-base units (`0` when unknown).
    pub duration: i64,
    /// Sync sample: the presenter restarts enqueue from a keyframe after a seek.
    pub is_key: bool,
}

/// A demuxer positioned on one container, yielding the selected video stream's
/// compressed packets. Owns its [`FfInput`]; drop tears the demuxer down.
pub struct VideoDemuxer {
    input: FfInput<'static>,
    info: DemuxStreamInfo,
    packet: ff::Packet,
    eof: bool,
}

impl VideoDemuxer {
    /// Open `input` and read the stream facts. `cancel` is the shared teardown
    /// flag (flipped on stop): a blocking read then aborts inside libav.
    pub fn open(input: &VideoInput, cancel: Arc<AtomicBool>) -> Result<Self, String> {
        let mut ff_in = FfInput::open(input, None)?;
        ff_in.set_cancel(cancel);
        let facts = video_facts(ff_in.ctx())?;
        let info = build_stream_info(facts, ff_in.ctx())?;
        Ok(Self {
            input: ff_in,
            info,
            packet: ff::Packet::empty(),
            eof: false,
        })
    }

    /// The one-time stream facts (codec, dims, extradata, DoVi config).
    pub fn info(&self) -> &DemuxStreamInfo {
        &self.info
    }

    /// Read the next compressed packet on the selected video stream. `Ok(None)`
    /// at end-of-stream (the demuxer stays open — a seek re-arms it). Packets on
    /// other streams (audio/subtitle) are skipped.
    pub fn read_packet(&mut self) -> Result<Option<DemuxPacket>, String> {
        if self.eof {
            return Ok(None);
        }
        let index = self.info.facts.index;
        loop {
            self.input.set_op_deadline(Some(READ_DEADLINE));
            let r = self.packet.read(self.input.ctx());
            self.input.set_op_deadline(None);
            match r {
                Ok(()) => {
                    if self.packet.stream() != index {
                        continue; // audio / subtitle — not ours
                    }
                    let data = self.packet.data().map(<[u8]>::to_vec).unwrap_or_default();
                    if data.is_empty() {
                        continue; // an empty packet carries nothing to enqueue
                    }
                    return Ok(Some(DemuxPacket {
                        data,
                        pts: self.packet.pts(),
                        dts: self.packet.dts(),
                        duration: self.packet.duration(),
                        is_key: self.packet.is_key(),
                    }));
                }
                Err(ff::Error::Eof) => {
                    self.eof = true;
                    return Ok(None);
                }
                Err(ff::Error::Other { errno }) if errno == ff::util::error::EAGAIN => {
                    if self.input.cancelled() {
                        return Err("cancelled".into());
                    }
                    continue;
                }
                Err(e) => return Err(format!("demux read: {e}")),
            }
        }
    }

    /// Seek to a keyframe at or before `target` (stream units derived from the
    /// container start offset, as the producer does), so the presenter can flush
    /// its renderer and re-enqueue from a clean IDR. Clears the EOF latch.
    pub fn seek(&mut self, target: Duration) -> Result<(), String> {
        let facts = &self.info.facts;
        let base = facts.start_time.unwrap_or(0);
        let target_units = base.saturating_add(facts.duration_to_pts(target));
        let index = facts.index as i32;
        self.input.set_op_deadline(Some(READ_DEADLINE));
        let rc = unsafe {
            ff::ffi::avformat_seek_file(
                self.input.ctx().as_mut_ptr(),
                index,
                i64::MIN,
                target_units,
                target_units,
                0,
            )
        };
        let rc = if rc < 0 {
            // Nothing at/before target (a start-offset edge): allow landing past
            // it — the presenter reveals on the first frame decoded from there.
            unsafe {
                ff::ffi::avformat_seek_file(
                    self.input.ctx().as_mut_ptr(),
                    index,
                    i64::MIN,
                    target_units,
                    i64::MAX,
                    0,
                )
            }
        } else {
            rc
        };
        self.input.set_op_deadline(None);
        if rc < 0 {
            return Err(format!("demux seek failed: {}", ff::Error::from(rc)));
        }
        self.eof = false;
        Ok(())
    }
}

/// Read the codec, extradata, NAL length-prefix size, and Dolby Vision config
/// for the selected stream. Borrows the already-probed `facts` so it needn't
/// re-select the stream.
fn build_stream_info(
    facts: VideoFacts,
    ctx: &ff::format::context::Input,
) -> Result<DemuxStreamInfo, String> {
    let stream = ctx
        .streams()
        .find(|s| s.index() == facts.index)
        .ok_or("selected video stream vanished")?;
    let par = stream.parameters();
    let codec = match par.id() {
        ff::codec::Id::H264 => VideoCodec::H264,
        ff::codec::Id::HEVC => VideoCodec::Hevc,
        _ => VideoCodec::Other,
    };
    let extradata = read_extradata(&stream);
    let (length_prefixed, nal_length_size) = parse_nal_length(codec, &extradata);
    let dovi = read_dovi(&stream);
    Ok(DemuxStreamInfo {
        facts,
        codec,
        extradata,
        nal_length_size,
        length_prefixed,
        dovi,
    })
}

/// Copy the stream's codec-private data (the `hvcC`/`avcC` atom, or Annex B
/// parameter sets for TS). Empty when the container declares none.
fn read_extradata(stream: &ff::format::stream::Stream) -> Vec<u8> {
    unsafe {
        let p = stream.parameters().as_ptr();
        if p.is_null() {
            return Vec::new();
        }
        let size = (*p).extradata_size;
        let data = (*p).extradata;
        if data.is_null() || size <= 0 {
            return Vec::new();
        }
        std::slice::from_raw_parts(data, size as usize).to_vec()
    }
}

/// Whether `extradata` is a length-prefixed configuration atom and, if so, the
/// NAL length size it declares. Annex B extradata (start-code delimited) or a
/// too-short atom → `(false, 0)`, which routes the clip to the Session fallback.
fn parse_nal_length(codec: VideoCodec, extradata: &[u8]) -> (bool, u8) {
    // Annex B parameter sets begin with a start code — not a config atom.
    let starts_with_start_code =
        extradata.starts_with(&[0, 0, 0, 1]) || extradata.starts_with(&[0, 0, 1]);
    if extradata.is_empty() || starts_with_start_code {
        return (false, 0);
    }
    match codec {
        // hvcC: `lengthSizeMinusOne` is the low 2 bits of byte 21.
        VideoCodec::Hevc if extradata.len() > 21 => (true, (extradata[21] & 0x3) + 1),
        // avcC: `lengthSizeMinusOne` is the low 2 bits of byte 4.
        VideoCodec::H264 if extradata.len() > 4 => (true, (extradata[4] & 0x3) + 1),
        _ => (false, 0),
    }
}

/// Parse the Dolby Vision configuration record from the stream's coded side
/// data (`AV_PKT_DATA_DOVI_CONF`), mirroring the `codecpar` side-data walk in
/// [`super::probe::rotation_degrees`]. `None` when the stream carries no DoVi
/// config (the common SDR/HDR10 case). The record's first eight fields are the
/// stable `AVDOVIDecoderConfigurationRecord` ABI prefix (all `uint8_t`, no
/// padding between them) — the struct isn't in the bindgen allowlist, so we
/// read the bytes directly, exactly as `color.rs` mirrors the HDR metadata.
fn read_dovi(stream: &ff::format::stream::Stream) -> Option<DoviConfig> {
    use ffmpeg_next::ffi;
    unsafe {
        let par = (*stream.as_ptr()).codecpar;
        if par.is_null() {
            return None;
        }
        let sd = ffi::av_packet_side_data_get(
            (*par).coded_side_data,
            (*par).nb_coded_side_data,
            ffi::AVPacketSideDataType::AV_PKT_DATA_DOVI_CONF,
        );
        if sd.is_null() || (*sd).data.is_null() || (*sd).size < 8 {
            return None;
        }
        let d = std::slice::from_raw_parts((*sd).data, (*sd).size as usize);
        Some(build_dovi(
            d[0],
            d[1],
            d[2],
            d[3],
            d[4] != 0,
            d[5] != 0,
            d[6] != 0,
            d[7],
        ))
    }
}

/// Assemble a [`DoviConfig`] (including the packed 24-byte configuration box)
/// from the record fields. Split out so it is unit-testable without a DoVi clip.
#[allow(clippy::too_many_arguments)]
fn build_dovi(
    version_major: u8,
    version_minor: u8,
    profile: u8,
    level: u8,
    rpu_present: bool,
    el_present: bool,
    bl_present: bool,
    bl_signal_compatibility_id: u8,
) -> DoviConfig {
    // DOVIDecoderConfigurationRecord: dv_version_major(8) dv_version_minor(8),
    // then a big-endian bit-packed word: dv_profile(7) dv_level(6) rpu(1) el(1)
    // bl(1) bl_signal_compatibility_id(4) reserved(...). 24 bytes total, tail 0.
    let word: u32 = ((profile as u32 & 0x7f) << 25)
        | ((level as u32 & 0x3f) << 19)
        | ((rpu_present as u32) << 18)
        | ((el_present as u32) << 17)
        | ((bl_present as u32) << 16)
        | ((bl_signal_compatibility_id as u32 & 0x0f) << 12);
    let mut box_payload = [0u8; 24];
    box_payload[0] = version_major;
    box_payload[1] = version_minor;
    box_payload[2..6].copy_from_slice(&word.to_be_bytes());
    // dvvC for the profile-8.x/9.x records (they carry bl_signal_compatibility_id);
    // legacy dvcC otherwise — matching FFmpeg movenc.
    let atom = if profile > 7 { *b"dvvC" } else { *b"dvcC" };
    DoviConfig {
        version_major,
        version_minor,
        profile,
        level,
        rpu_present,
        el_present,
        bl_present,
        bl_signal_compatibility_id,
        atom,
        box_payload,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> VideoInput {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/video")
            .join(name);
        VideoInput::Path(p)
    }

    fn cancel() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    #[test]
    fn opens_h264_mkv_and_reads_length_prefixed_packets() {
        // MKV is the flagship container the sample-buffer path targets.
        let mut dx = VideoDemuxer::open(&fixture("black_then_color.mkv"), cancel()).unwrap();
        let info = dx.info();
        assert_eq!(info.codec, VideoCodec::H264);
        assert_eq!((info.facts.width, info.facts.height), (64, 64));
        // avcC atom present and length-prefixed (the CMSampleBuffer wants this).
        assert!(!info.extradata.is_empty(), "avcC extradata must be present");
        assert!(info.length_prefixed, "MKV H.264 is length-prefixed");
        assert!(
            (1..=4).contains(&info.nal_length_size),
            "nal length size {} out of range",
            info.nal_length_size
        );
        assert!(info.dovi.is_none(), "plain H.264 has no DoVi config");

        // First packet is a keyframe (an IDR opens the stream) with real bytes.
        let first = dx.read_packet().unwrap().expect("at least one packet");
        assert!(!first.data.is_empty());
        assert!(first.is_key, "the opening access unit is a keyframe");

        // Drain: at least a handful of packets, and at most one keyframe run.
        let mut count = 1usize;
        while let Some(_p) = dx.read_packet().unwrap() {
            count += 1;
            if count > 10_000 {
                break;
            }
        }
        assert!(count >= 2, "expected multiple packets, got {count}");
        // Past EOF stays EOF (idempotent None).
        assert!(dx.read_packet().unwrap().is_none());
    }

    #[test]
    fn opens_hevc_mp4_with_hvcc_extradata() {
        let dx = VideoDemuxer::open(&fixture("hdr_pq.mp4"), cancel()).unwrap();
        let info = dx.info();
        assert_eq!(info.codec, VideoCodec::Hevc);
        assert!(!info.extradata.is_empty(), "hvcC extradata must be present");
        assert!(info.length_prefixed);
        // hvcC configurationVersion byte is 1 (a sanity check that we copied the
        // atom, not Annex B).
        assert_eq!(info.extradata[0], 1, "hvcC configurationVersion");
        assert!((1..=4).contains(&info.nal_length_size));
    }

    #[test]
    fn seek_returns_to_readable_keyframe() {
        let mut dx = VideoDemuxer::open(&fixture("black_then_color.mkv"), cancel()).unwrap();
        // Read some packets, then seek back to the start.
        for _ in 0..3 {
            let _ = dx.read_packet().unwrap();
        }
        dx.seek(Duration::ZERO).unwrap();
        let p = dx
            .read_packet()
            .unwrap()
            .expect("a packet after seek-to-zero");
        assert!(!p.data.is_empty());
        assert!(p.is_key, "a seek lands the demuxer on a keyframe");
    }

    #[test]
    fn dovi_box_packs_profile_8_1() {
        // Corpus profile 8.1: BL+RPU, no EL, HDR10-compatible base
        // (bl_signal_compatibility_id = 1). Verify the bit-packed box.
        let c = build_dovi(1, 0, 8, 6, true, false, true, 1);
        assert_eq!(&c.atom, b"dvvC", "profile 8 uses the dvvC box");
        assert_eq!(c.box_payload[0], 1); // version major
        assert_eq!(c.box_payload[1], 0); // version minor

        // word = profile<<25 | level<<19 | rpu<<18 | bl<<16 | compat<<12
        let word = u32::from_be_bytes([
            c.box_payload[2],
            c.box_payload[3],
            c.box_payload[4],
            c.box_payload[5],
        ]);
        assert_eq!((word >> 25) & 0x7f, 8, "profile");
        assert_eq!((word >> 19) & 0x3f, 6, "level");
        assert_eq!((word >> 18) & 1, 1, "rpu_present");
        assert_eq!((word >> 17) & 1, 0, "el_present");
        assert_eq!((word >> 16) & 1, 1, "bl_present");
        assert_eq!((word >> 12) & 0x0f, 1, "bl_signal_compatibility_id");
        // Tail is reserved-zero.
        assert!(c.box_payload[6..].iter().all(|&b| b == 0));
    }

    #[test]
    fn dovi_legacy_profile_uses_dvcc() {
        let c = build_dovi(1, 0, 5, 6, true, false, true, 0);
        assert_eq!(&c.atom, b"dvcC", "profile <= 7 uses the legacy dvcC box");
    }

    /// Verify DoVi extraction end-to-end on a real Dolby Vision clip. Point
    /// `PB_DOVI_TEST` at a DoVi profile-8.x container (e.g. the 0A remux control
    /// `~/Downloads/pb-remux-control-video-only.mp4`, which preserves the `dvvC`
    /// via `-c copy`). De-risks the 0C gate: it proves the config the Swift
    /// presenter needs actually reaches Rust.
    #[test]
    #[ignore = "needs PB_DOVI_TEST pointing at a Dolby Vision clip"]
    fn dovi_config_from_real_clip() {
        let Ok(path) = std::env::var("PB_DOVI_TEST") else {
            eprintln!("skipping: set PB_DOVI_TEST to a Dolby Vision container");
            return;
        };
        let dx = VideoDemuxer::open(&VideoInput::Path(path.into()), cancel()).unwrap();
        let info = dx.info();
        eprintln!(
            "codec={:?} dims={}x{} nal_len={} length_prefixed={}",
            info.codec,
            info.facts.width,
            info.facts.height,
            info.nal_length_size,
            info.length_prefixed
        );
        let dovi = info.dovi.as_ref().expect("DoVi config must be extracted");
        eprintln!(
            "DoVi: profile={} level={} bl={} rpu={} el={} compat_id={} atom={}",
            dovi.profile,
            dovi.level,
            dovi.bl_present,
            dovi.rpu_present,
            dovi.el_present,
            dovi.bl_signal_compatibility_id,
            std::str::from_utf8(&dovi.atom).unwrap(),
        );
        assert_eq!(info.codec, VideoCodec::Hevc);
        assert!(dovi.profile > 0, "a real DoVi profile");
        assert!(dovi.bl_present, "the base layer must be present");
    }

    #[test]
    fn annexb_extradata_is_not_length_prefixed() {
        let annexb = [0u8, 0, 0, 1, 0x67, 0x42];
        assert_eq!(parse_nal_length(VideoCodec::H264, &annexb), (false, 0));
        assert_eq!(parse_nal_length(VideoCodec::Hevc, &[]), (false, 0));
    }
}
