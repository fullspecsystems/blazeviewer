//! Demux/probe unit tests for the avis backend (task #76, plan phase 4/8).
//!
//! The synthetic-file builder below emits real ISOBMFF byte layouts (two-pass
//! so `stco` carries true absolute offsets), which keeps every table form and
//! every reject class testable on any platform with no fixture files and no
//! dav1d. Decode-integration tests against real encoded fixtures live in the
//! `av1_dav1d`-gated section at the bottom.

use super::*;

// ── Synthetic avis builder ───────────────────────────────────────────────────

struct Fx {
    /// ftyp major + compatible brands (minor version written as 0).
    brands: Vec<[u8; 4]>,
    handler: [u8; 4],
    entry_type: [u8; 4],
    extra_stsd_entry: bool,
    use_stz2: bool,
    /// `Some(sz)` = constant-sample-size stsz form.
    const_size: Option<u32>,
    nclx: Option<(u16, u16, u16, bool)>,
    timescale: u32,
    /// Per-sample payloads (also drive stsz when not const_size).
    samples: Vec<Vec<u8>>,
    /// Per-sample durations (must match samples length; drives stts runs).
    durations: Vec<u32>,
    /// Samples per chunk (must sum to samples.len()).
    chunks: Vec<usize>,
    co64: bool,
    add_moof: bool,
    /// Prepend an `auxv`-handler av01 trak (the alpha track shape).
    aux_trak_first: bool,
    config_obus: Vec<u8>,
    /// Shift every stco offset by this many bytes (corruption knob).
    offset_shift: i64,
}

impl Default for Fx {
    fn default() -> Self {
        Self {
            brands: vec![*b"avis", *b"avif", *b"msf1"], // real files list msf1 too
            handler: *b"pict",
            entry_type: *b"av01",
            extra_stsd_entry: false,
            use_stz2: false,
            const_size: None,
            nclx: None,
            timescale: 1000,
            samples: vec![vec![0xAA; 10], vec![0xBB; 20], vec![0xCC; 30]],
            durations: vec![40, 40, 80],
            chunks: vec![3],
            co64: false,
            add_moof: false,
            aux_trak_first: false,
            config_obus: vec![0x0A, 0x03, 0x00, 0x00, 0x00],
            offset_shift: 0,
        }
    }
}

fn bx(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + payload.len());
    v.extend_from_slice(&(payload.len() as u32 + 8).to_be_bytes());
    v.extend_from_slice(typ);
    v.extend_from_slice(payload);
    v
}

/// FullBox with version 0, flags 0.
fn fbx(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut p = vec![0u8; 4];
    p.extend_from_slice(payload);
    bx(typ, &p)
}

impl Fx {
    fn build(&self) -> Vec<u8> {
        // Pass 1 with zeroed chunk offsets to learn the mdat payload position,
        // pass 2 with the real ones (box sizes are offset-independent).
        let zeros = vec![0u64; self.chunks.len()];
        let pass1 = self.assemble(&zeros);
        let mdat_payload_at = pass1.len() - self.mdat_size() + 8;
        let mut offsets = Vec::new();
        let mut at = mdat_payload_at as i64 + self.offset_shift;
        let mut idx = 0usize;
        for &per in &self.chunks {
            offsets.push(at.max(0) as u64);
            for _ in 0..per {
                at += self.samples[idx].len() as i64;
                idx += 1;
            }
        }
        self.assemble(&offsets)
    }

    fn mdat_size(&self) -> usize {
        8 + self.samples.iter().map(Vec::len).sum::<usize>()
    }

    fn assemble(&self, chunk_offsets: &[u64]) -> Vec<u8> {
        // ftyp: major + minor(0) + compatibles.
        let mut ftyp = Vec::new();
        ftyp.extend_from_slice(&self.brands[0]);
        ftyp.extend_from_slice(&[0; 4]);
        for b in &self.brands[1..] {
            ftyp.extend_from_slice(b);
        }
        let ftyp = bx(b"ftyp", &ftyp);

        let mut traks = Vec::new();
        if self.aux_trak_first {
            traks.extend_from_slice(&self.trak(b"auxv", chunk_offsets));
        }
        traks.extend_from_slice(&self.trak(&self.handler, chunk_offsets));
        let moov = bx(b"moov", &traks);

        let mut mdat_payload = Vec::new();
        for s in &self.samples {
            mdat_payload.extend_from_slice(s);
        }
        let mdat = bx(b"mdat", &mdat_payload);

        let mut file = ftyp;
        if self.add_moof {
            file.extend_from_slice(&bx(b"moof", &[]));
        }
        file.extend_from_slice(&moov);
        file.extend_from_slice(&mdat);
        file
    }

    fn trak(&self, handler: &[u8; 4], chunk_offsets: &[u64]) -> Vec<u8> {
        // hdlr: pre_defined + handler_type + 12 reserved + empty name.
        let mut hdlr = vec![0u8; 4];
        hdlr.extend_from_slice(handler);
        hdlr.extend_from_slice(&[0u8; 12]);
        hdlr.push(0);
        let hdlr = fbx(b"hdlr", &hdlr);

        // mdhd v0: creation + modification + timescale + duration + lang/pre.
        let mut mdhd = Vec::new();
        mdhd.extend_from_slice(&[0u8; 8]);
        mdhd.extend_from_slice(&self.timescale.to_be_bytes());
        mdhd.extend_from_slice(&[0u8; 8]);
        let mdhd = fbx(b"mdhd", &mdhd);

        // av01 sample entry: 78 fixed VisualSampleEntry bytes + children.
        let mut entry = vec![0u8; 78];
        entry[7] = 1; // data_reference_index = 1
        let mut av1c = vec![0x81, 0x00, 0x00, 0x00];
        av1c.extend_from_slice(&self.config_obus);
        entry.extend_from_slice(&bx(b"av1C", &av1c));
        if let Some((prim, trc, mtx, full)) = self.nclx {
            let mut colr = Vec::new();
            colr.extend_from_slice(b"nclx");
            colr.extend_from_slice(&prim.to_be_bytes());
            colr.extend_from_slice(&trc.to_be_bytes());
            colr.extend_from_slice(&mtx.to_be_bytes());
            colr.push(if full { 0x80 } else { 0 });
            entry.extend_from_slice(&bx(b"colr", &colr));
        }
        let entry = bx(&self.entry_type, &entry);

        let mut stsd_payload = Vec::new();
        let count: u32 = if self.extra_stsd_entry { 2 } else { 1 };
        stsd_payload.extend_from_slice(&count.to_be_bytes());
        stsd_payload.extend_from_slice(&entry);
        if self.extra_stsd_entry {
            stsd_payload.extend_from_slice(&entry);
        }
        let stsd = fbx(b"stsd", &stsd_payload);

        // stts: compress consecutive equal durations into runs.
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for &d in &self.durations {
            match runs.last_mut() {
                Some((n, delta)) if *delta == d => *n += 1,
                _ => runs.push((1, d)),
            }
        }
        let mut stts = (runs.len() as u32).to_be_bytes().to_vec();
        for (n, d) in runs {
            stts.extend_from_slice(&n.to_be_bytes());
            stts.extend_from_slice(&d.to_be_bytes());
        }
        let stts = fbx(b"stts", &stts);

        // stsc: compress the chunks list into (first_chunk, per_chunk, 1) runs.
        let mut sruns: Vec<(u32, u32)> = Vec::new();
        for (i, &per) in self.chunks.iter().enumerate() {
            match sruns.last() {
                Some(&(_, p)) if p == per as u32 => {}
                _ => sruns.push((i as u32 + 1, per as u32)),
            }
        }
        let mut stsc = (sruns.len() as u32).to_be_bytes().to_vec();
        for (first, per) in sruns {
            stsc.extend_from_slice(&first.to_be_bytes());
            stsc.extend_from_slice(&per.to_be_bytes());
            stsc.extend_from_slice(&1u32.to_be_bytes());
        }
        let stsc = fbx(b"stsc", &stsc);

        // stsz (or an empty stz2 to trigger the reject).
        let sz_box = if self.use_stz2 {
            fbx(b"stz2", &[0, 0, 0, 8, 0, 0, 0, 0])
        } else {
            let mut stsz = Vec::new();
            stsz.extend_from_slice(&self.const_size.unwrap_or(0).to_be_bytes());
            stsz.extend_from_slice(&(self.samples.len() as u32).to_be_bytes());
            if self.const_size.is_none() {
                for s in &self.samples {
                    stsz.extend_from_slice(&(s.len() as u32).to_be_bytes());
                }
            }
            fbx(b"stsz", &stsz)
        };

        // stco / co64.
        let co_box = if self.co64 {
            let mut co = (chunk_offsets.len() as u32).to_be_bytes().to_vec();
            for &o in chunk_offsets {
                co.extend_from_slice(&o.to_be_bytes());
            }
            fbx(b"co64", &co)
        } else {
            let mut co = (chunk_offsets.len() as u32).to_be_bytes().to_vec();
            for &o in chunk_offsets {
                co.extend_from_slice(&(o as u32).to_be_bytes());
            }
            fbx(b"stco", &co)
        };

        let mut stbl = stsd;
        stbl.extend_from_slice(&stts);
        stbl.extend_from_slice(&stsc);
        stbl.extend_from_slice(&sz_box);
        stbl.extend_from_slice(&co_box);
        let stbl = bx(b"stbl", &stbl);

        let minf = bx(b"minf", &stbl);
        let mut mdia = hdlr;
        mdia.extend_from_slice(&mdhd);
        mdia.extend_from_slice(&minf);
        let mdia = bx(b"mdia", &mdia);
        bx(b"trak", &mdia)
    }
}

// ── Positive demux tests ─────────────────────────────────────────────────────

#[test]
fn basic_avis_round_trips() {
    let fx = Fx::default();
    let file = fx.build();
    let info = probe_avis(&file).expect("probe");
    assert_eq!(info.timescale, 1000);
    assert_eq!(info.config_obus, fx.config_obus);
    assert!(!info.truncated);
    assert_eq!(info.samples.len(), 3);
    // Sizes + durations straight from the tables; offsets index real payload.
    for (i, (want_len, want_dur)) in [(10usize, 40u64), (20, 40), (30, 80)].iter().enumerate() {
        let s = info.samples[i];
        assert_eq!(s.size, *want_len);
        assert_eq!(s.duration_ticks, *want_dur);
        let body = &file[s.offset..s.offset + s.size];
        let expect = [0xAA, 0xBB, 0xCC][i];
        assert!(body.iter().all(|&b| b == expect), "sample {i} payload");
    }
}

#[test]
fn constant_stsz_and_multi_chunk_stsc() {
    let fx = Fx {
        samples: vec![vec![1u8; 16]; 5],
        durations: vec![33; 5],
        const_size: Some(16),
        chunks: vec![2, 2, 1], // multi-run stsc: (1,2), (3,1)
        ..Fx::default()
    };
    let file = fx.build();
    let info = probe_avis(&file).expect("probe");
    assert_eq!(info.samples.len(), 5);
    assert!(info.samples.iter().all(|s| s.size == 16));
    // Chunks are contiguous runs in mdat here, so offsets are strictly
    // increasing by 16.
    for w in info.samples.windows(2) {
        assert_eq!(w[1].offset - w[0].offset, 16);
    }
}

#[test]
fn co64_offsets_work() {
    let fx = Fx {
        co64: true,
        ..Fx::default()
    };
    let info = probe_avis(&fx.build()).expect("probe");
    assert_eq!(info.samples.len(), 3);
}

#[test]
fn variable_durations_survive() {
    let fx = Fx {
        durations: vec![20, 100, 20],
        ..Fx::default()
    };
    let info = probe_avis(&fx.build()).expect("probe");
    let d: Vec<u64> = info.samples.iter().map(|s| s.duration_ticks).collect();
    assert_eq!(d, [20, 100, 20]);
}

#[test]
fn track_scoped_nclx_is_parsed_and_p3_transform_carried() {
    // Display-P3-ish nclx: primaries 12, sRGB transfer 13, matrix 6, full.
    let fx = Fx {
        nclx: Some((12, 13, 6, true)),
        ..Fx::default()
    };
    let info = probe_avis(&fx.build()).expect("probe");
    let n = info.nclx.expect("nclx captured");
    assert_eq!(
        (n.primaries, n.transfer, n.matrix, n.full_range),
        (12, 13, 6, true)
    );
    assert!(info.color.is_some(), "P3 must carry a display transform");
}

#[test]
fn srgb_nclx_is_passthrough_but_still_drives_yuv() {
    // sRGB nclx: no display transform, but matrix/range still captured — the
    // exact case where color_from_colr_box alone would lose information.
    let fx = Fx {
        nclx: Some((1, 13, 1, false)),
        ..Fx::default()
    };
    let info = probe_avis(&fx.build()).expect("probe");
    assert!(
        info.color.is_none(),
        "sRGB display transform is passthrough"
    );
    let n = info.nclx.expect("nclx captured");
    assert_eq!((n.matrix, n.full_range), (1, false));
}

#[test]
fn alpha_aux_trak_is_skipped_for_the_pict_track() {
    let fx = Fx {
        aux_trak_first: true,
        nclx: Some((12, 13, 6, true)),
        ..Fx::default()
    };
    let info = probe_avis(&fx.build()).expect("probe");
    // The pict trak's tables won (3 samples), not a reject or the aux trak.
    assert_eq!(info.samples.len(), 3);
    assert!(info.nclx.is_some());
}

#[test]
fn empty_config_obus_are_allowed() {
    let fx = Fx {
        config_obus: Vec::new(),
        ..Fx::default()
    };
    let info = probe_avis(&fx.build()).expect("probe");
    assert!(info.config_obus.is_empty());
}

#[test]
fn max_frames_truncates_the_table() {
    let n = crate::animation::MAX_FRAMES + 100;
    let fx = Fx {
        samples: vec![vec![7u8; 1]; n],
        durations: vec![33; n],
        const_size: Some(1),
        chunks: vec![n],
        ..Fx::default()
    };
    let info = probe_avis(&fx.build()).expect("probe");
    assert!(info.truncated);
    assert_eq!(info.samples.len(), crate::animation::MAX_FRAMES);
}

// ── Reject classes ───────────────────────────────────────────────────────────

fn reject_reason(fx: &Fx) -> &'static str {
    match probe_avis(&fx.build()) {
        Ok(_) => panic!("must reject"),
        Err(e) => e,
    }
}

#[test]
fn msf1_without_avis_is_rejected() {
    let fx = Fx {
        brands: vec![*b"msf1", *b"heic"],
        ..Fx::default()
    };
    assert_eq!(reject_reason(&fx), "no avis brand");
}

#[test]
fn still_avif_is_rejected() {
    let fx = Fx {
        brands: vec![*b"avif", *b"mif1"],
        ..Fx::default()
    };
    assert_eq!(reject_reason(&fx), "no avis brand");
}

#[test]
fn fragmented_moof_is_rejected() {
    let fx = Fx {
        add_moof: true,
        ..Fx::default()
    };
    assert_eq!(reject_reason(&fx), "fragmented (moof) not supported");
}

#[test]
fn encrypted_entry_is_rejected() {
    let fx = Fx {
        entry_type: *b"encv",
        ..Fx::default()
    };
    assert_eq!(reject_reason(&fx), "encrypted track");
}

#[test]
fn multiple_sample_descriptions_are_rejected() {
    let fx = Fx {
        extra_stsd_entry: true,
        ..Fx::default()
    };
    assert_eq!(reject_reason(&fx), "multiple sample descriptions");
}

#[test]
fn stz2_is_rejected() {
    let fx = Fx {
        use_stz2: true,
        ..Fx::default()
    };
    assert_eq!(reject_reason(&fx), "compact stz2 not supported");
}

#[test]
fn hdr_pq_and_hlg_are_rejected() {
    for trc in [16u16, 18] {
        let fx = Fx {
            nclx: Some((9, trc, 9, false)),
            ..Fx::default()
        };
        assert_eq!(reject_reason(&fx), "hdr transfer (pq/hlg)");
    }
}

#[test]
fn zero_timescale_is_rejected() {
    let fx = Fx {
        timescale: 0,
        ..Fx::default()
    };
    assert_eq!(reject_reason(&fx), "mdhd timescale is zero");
}

#[test]
fn out_of_range_sample_is_rejected() {
    let fx = Fx {
        offset_shift: 10_000,
        ..Fx::default()
    };
    assert_eq!(reject_reason(&fx), "sample out of range");
}

#[test]
fn aux_only_file_has_no_color_track() {
    let fx = Fx {
        handler: *b"auxv",
        ..Fx::default()
    };
    assert_eq!(reject_reason(&fx), "no av01 pict track");
}

#[test]
fn garbage_and_truncation_never_panic() {
    // Random-ish corruptions of a valid file must all yield Err, never panic.
    let file = Fx::default().build();
    for cut in [0, 1, 7, 8, 9, 15, 40, file.len() / 2, file.len() - 1] {
        let _ = probe_avis(&file[..cut]);
    }
    let mut junk = file.clone();
    for i in (0..junk.len()).step_by(7) {
        junk[i] ^= 0x5A;
        let _ = probe_avis(&junk);
    }
}

// ── Decode integration (real encoded fixtures; needs linked dav1d) ──────────
// Fixture generation is documented in tests/fixtures/avis/README.md.
#[cfg(av1_dav1d)]
mod decode_integration {
    use super::super::decode_avis;
    use std::sync::atomic::AtomicBool;

    fn fixture(name: &str) -> Vec<u8> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/avis")
            .join(name);
        std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
    }

    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn assert_rgb_close(got: [u8; 3], want: [u8; 3], tol: u8, what: &str) {
        for c in 0..3 {
            assert!(
                got[c].abs_diff(want[c]) <= tol,
                "{what}: got {got:?}, want {want:?} ±{tol}"
            );
        }
    }

    #[test]
    fn rgb_8bit_420_solid_frames() {
        let bytes = fixture("rgb_8bit_420.avif");
        let anim = decode_avis(&bytes, None, &no_cancel()).expect("decode");
        assert_eq!(anim.frames.len(), 3, "three solid frames");
        assert_eq!((anim.width, anim.height), (64, 64));
        assert_eq!(anim.loop_count, 0, "avis loops like a GIF");
        assert!(!anim.truncated);
        let want = [[255u8, 0, 0], [0, 255, 0], [0, 0, 255]];
        for (i, f) in anim.frames.iter().enumerate() {
            assert_rgb_close(super::tests_mean(f), want[i], 12, &format!("frame {i}"));
            assert!(
                f.delay.as_millis() > 50 && f.delay.as_millis() < 500,
                "sane delay"
            );
        }
    }

    #[test]
    fn ten_bit_fixture_decodes() {
        let bytes = fixture("rgb_10bit_420.avif");
        let anim = decode_avis(&bytes, None, &no_cancel()).expect("decode 10-bit");
        assert_eq!(anim.frames.len(), 3);
        assert_rgb_close(
            super::tests_mean(&anim.frames[0]),
            [255, 0, 0],
            12,
            "10-bit red",
        );
    }

    #[test]
    fn odd_dimensions_decode() {
        let bytes = fixture("odd_63x37.avif");
        let anim = decode_avis(&bytes, None, &no_cancel()).expect("decode odd dims");
        assert_eq!((anim.width, anim.height), (63, 37));
        assert!(anim.frames.len() >= 2);
    }

    #[test]
    fn yuv444_fixture_decodes() {
        let bytes = fixture("rgb_8bit_444.avif");
        let anim = decode_avis(&bytes, None, &no_cancel()).expect("decode 444");
        assert_rgb_close(
            super::tests_mean(&anim.frames[0]),
            [255, 0, 0],
            12,
            "444 red",
        );
    }

    #[test]
    fn alpha_aux_track_plays_opaque_from_the_color_track() {
        let bytes = fixture("alpha_64x64.avif");
        let anim = decode_avis(&bytes, None, &no_cancel()).expect("decode alpha avis");
        assert!(anim.frames.len() >= 2);
        // v1 policy: alpha ignored, fully opaque output.
        assert!(anim.frames[0].rgba.chunks_exact(4).all(|px| px[3] == 255));
    }

    #[test]
    fn variable_durations_ride_through() {
        let bytes = fixture("vardur_64x64.avif");
        let anim = decode_avis(&bytes, None, &no_cancel()).expect("decode vardur");
        let mut delays: Vec<u128> = anim.frames.iter().map(|f| f.delay.as_millis()).collect();
        delays.sort_unstable();
        delays.dedup();
        assert!(
            delays.len() >= 2,
            "distinct per-frame delays, got {delays:?}"
        );
    }

    #[test]
    fn p3_fixture_carries_a_display_transform() {
        let bytes = fixture("p3_64x64.avif");
        let anim = decode_avis(&bytes, None, &no_cancel()).expect("decode p3");
        assert!(
            anim.color.enabled,
            "P3 avis must carry a real display transform, not sRGB passthrough"
        );
    }

    #[test]
    fn hdr_fixture_is_rejected_by_the_probe() {
        let bytes = fixture("hdr_pq_64x64.avif");
        assert!(
            super::super::probe_avis(&bytes).is_err(),
            "PQ avis → still path"
        );
        assert!(decode_avis(&bytes, None, &no_cancel()).is_err());
    }

    #[test]
    fn preset_cancel_stops_before_completion() {
        let bytes = fixture("rgb_8bit_420.avif");
        let cancel = AtomicBool::new(true);
        let r = decode_avis(&bytes, None, &cancel);
        assert!(r.is_err(), "pre-set cancel flag must abort the decode");
    }
}

/// Shared helper for the integration tests: mean RGB of a frame.
#[cfg(av1_dav1d)]
pub(crate) fn tests_mean(f: &crate::animation::AnimFrame) -> [u8; 3] {
    let mut sums = [0u64; 3];
    for px in f.rgba.chunks_exact(4) {
        for c in 0..3 {
            sums[c] += px[c] as u64;
        }
    }
    let n = (f.rgba.len() / 4) as u64;
    [0, 1, 2].map(|c| (sums[c] / n.max(1)) as u8)
}
