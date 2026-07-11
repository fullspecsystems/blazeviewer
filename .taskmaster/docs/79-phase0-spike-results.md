# Task #79 phase 0 — Windows MF spike results (2026-07-11)

Harness: `cargo run --release -p pb-decode --example video_probe -- spike <file> [--fit W H]
[--frames N] [--dump <dir>]` and `… -- sweep <file>...`. All numbers from the target machine
(RTX 5090 box, media on `D:`), release build, real library footage plus ffmpeg-generated
fixtures (scratchpad; regen commands in the shell history / trivially re-creatable —
`testsrc2` 2 s clips per container, x265 10-bit HLG, `-display_rotation` remux).

## Verdict up front

**No blockers. The Windows producer can be built on the sync `IMFSourceReader` exactly as
planned**, with two design consequences the plan must absorb:

1. **Seek = fresh reader, never a hot one.** `SetCurrentPosition` on a reader whose internal
   pipeline is warm blocks **~1.0 s on HEVC** (the Store-extension MFT); on a fresh reader
   it's **~0.2 ms**, and open+negotiate is only **4–20 ms**. H.264 (in-box decoder) never
   shows the ~1 s cost (10–19 ms warm). So the seek path recreates the reader (or seeks
   before the first read), and the discarded hot reader goes to the retirement pool.
2. **Teardown cost is codec-dependent:** dropping a mid-stream reader blocks **~1.0–1.1 s on
   HEVC**, **~43 ms on H.264** (matches the `mf_video.rs:186` observation). The plan's split
   criteria (UI-cancel within a frame / bounded background retirement, never joined) is
   confirmed necessary and sufficient.

## Decode throughput (120-frame pumps, wall-clock totals)

| Clip | Codec/geometry | ms/frame (pump total) | copy p50 (BGRX→RGBA) | read p50 |
|---|---|---|---|---|
| IMG_0393.mov (Live Photo) | H.264 720×960 12 fps | ~2.0 | 0.83 ms | 0.01 ms |
| IMG_1853.MOV (6 min) | HEVC 1920×1088 60 fps | **4.1** | 3.3 ms | 0.08 ms |
| IMG_0561.MOV (7.7 min) | HEVC 1920×1088 60 fps | 4.6 | 3.5 ms | 0.11 ms |
| RPReplay…mp4 (screen rec) | H.264 1920×1440 VFR | 4.6 | 3.5 ms | 0.04 ms |
| IMG_0060.MOV (93 s) | **HEVC 3840×2160 60 fps** | **14.5** | 12.6 ms | 1.8 ms |

- 4K60 HEVC decodes at ~69 fps sustained → **4K30 is comfortable, 4K60 is borderline** on
  one reader thread. read p50 ≈ 0 shows the Source Reader pipelines decode ahead of the
  consumer; the consumer-side **BGRX→RGBA copy is the dominant per-frame CPU cost**
  (12.6 ms at 4K). Phase-3/4 should keep the copy off the present path (producer thread)
  and consider SIMD/row-memcpy-with-shuffle if 4K60 matters.
- Occasional read max ≈ 30–100 ms spikes (GOP boundaries / readahead refill) — absorbed by
  the 2–3-frame queue + rebuffer policy.

## Fitted-output scaling (MF video processor)

`MF_MT_FRAME_SIZE` on the RGB32 output type **is honored** (asked 2560×1440 from a 4K
source → got exactly that, real scaled pixels). But it is **not free**: on the 4K clip,
read p50 went 0.13 → 8.0 ms while copy halved (12.4 → 6.7 ms); pump totals were a wash
(1727 ms native vs 1949 ms fitted for 120 frames). Conclusion:

- **The win vs the *current Live-Photo path* (full-res copy + per-frame Lanczos) is real** —
  Lanczos at 4K is far more than the ~13 ms copy, and it disappears entirely.
- Scale-in-processor vs copy-at-native is roughly cost-neutral; pick fitted output for the
  smaller queue bytes (a 1440p frame is 14 MB vs 33 MB — more lookahead per budget) and the
  no-Lanczos guarantee. On the 7680×2160 display a landscape 4K clip fits un-scaled anyway.

## Seek (SetCurrentPosition → decode-forward landing)

| Scenario | SetCurrentPosition | total land | discarded frames |
|---|---|---|---|
| HEVC 4K, fresh reader (E) | 0.2 ms | **345 ms** | 29 |
| HEVC 4K, warm reader | ~1015 ms | ~1.3 s | 29 |
| HEVC 1080p60, warm reader | ~1010 ms | ~1.2 s | 46 |
| H.264 1080p, fresh (E) | 0.2 ms | **54 ms** | 29 |
| H.264 1080p, warm | 10–19 ms | 60–106 ms | 29–62 |

- Landing accuracy: within +8…+24 ms of target everywhere (one frame interval — the plan's
  documented tolerance holds).
- Land time ≈ keyframe→target decode-forward at the throughput above (~GOP/2 × ms/frame);
  at 4K HEVC that's ~350 ms — fine for a coarse player with latest-value coalescing.
- **Seeking past EOS fails with `0xC00D36E5` (offset not permitted)** instead of clamping —
  the session must clamp targets to the seekable range first (plan seek-spec step 1 is
  load-bearing, not defensive).

## PTS

- Timestamps are real and monotonic on every clip tested; CFR clips show constant deltas
  (16.68 ms @60, 33.33 ms @30), the VFR screen recording reports its true average rate
  (46.39 fps) via `MF_MT_FRAME_RATE`.
- **Nonzero start PTS exists in the wild: MPEG-TS first frame = 766.67 ms.** The
  session-relative PTS origin normalization in the frame contract is required, not
  theoretical. (All .mov/.mp4 tested start at 0.)

## Container/codec sweep (this machine: Win 11 + HEVC/AV1/VP9/Web-Media/MPEG-2 Store extensions all installed)

Everything opened, negotiated RGB32, and produced a frame: mp4 (H.264/AV1), mov
(H.264/HEVC/HLG-HEVC), **mkv (H.264 + HEVC — the native byte-stream handler is real)**,
webm (VP8 + VP9), avi (MPEG-4 pt2), wmv (WMV2), mpg (MPEG-2), mts (H.264 TS), 3gp (H.263).

- ⚠ This box has **every** relevant Store extension installed (HEVC 2.5.10, AV1 2.0.7,
  VP9 1.2.20, Web Media 2.1.26, MPEG-2 1.2.13) — this sweep proves the *ceiling*, not the
  out-of-box floor. The **runtime-probe design is exactly right**: capability is per-machine,
  the poster attempt is the probe, and the "install the … extension" error text carries the
  fix. A clean-VM sweep (no extensions) is the remaining matrix measurement — cheap, not
  blocking (error paths already exercise gracefully: probe reports the failing *stage*).
- `MF_E_UNSUPPORTED_BYTESTREAM_TYPE` (no container handler) vs `MF_E_TOPO_CODEC_NOT_FOUND`
  (container OK, codec missing) are distinguishable → the error UI can be precise.

## Color / rotation / audio detection

- SDR iPhone footage reports prim=2/trans=5/matrix=1/range=2 (BT.709 limited) — the
  processor converts matrix+range for RGB32 out; primaries ride the existing
  `native_color` → `ColorTransform` in-shader path (same as Live Photos).
- **HLG 10-bit BT.2020 (prim=9, trans=16, matrix=4) negotiates RGB32 and decodes to
  plausible, saturated SDR** (eyeballed vs an SDR reference of the same test pattern; not
  gray/washed = not an identity punt). Tier-2 SDR guarantee is implementable as planned:
  8-bit path SDR-clamps HLG/PQ by design, primaries corrected in-shader, poster ≡ playback
  by construction (same reader config). Caveat: the library has **no real HLG phone
  footage** (2015–2021 iPhones here are all SDR BT.709) — synthetic fixture used; grab a
  real HLG/Dolby-Vision clip for the corpus when one exists.
- Rotation: `MF_MT_VIDEO_ROTATION` reads back (remuxed `-display_rotation 90` → rot=270 CW
  — note the CCW/CW sign flip); with advanced video processing the processor pre-rotates,
  same as Live Photos. Portrait-shot iPhone videos in this library store portrait
  dimensions natively (rot=0).
- Audio presence: `GetNativeMediaType(FIRST_AUDIO_STREAM, 0)` Ok/Err is a reliable
  `has_audio` (correct on every clip, including audio-less fixtures).
- Stream deselection (deselect all → select video) worked everywhere — no sample queuing,
  no surprises; one benign `MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED` tick at stream start
  on every file (already handled header-from-first-frame in the Live-Photo path).

## Open + first frame (the `P → first moving frame` budget)

Reader open + negotiate: **4–23 ms** on every container including the 5.9 GB file. First
ReadSample ≈ 30–100 ms. Preroll (2 frames) lands well under ~150 ms for 1080p, ~250 ms for
4K HEVC → the responsiveness acceptance (p50/p95 recorded per codec in phase 7) looks easily
achievable.

## What this changes in the plan

1. Seek spec step 3 becomes "**recreate the reader** (open ≈ 20 ms) and set position before
   the first read" on Windows; repositioning a warm reader is the fallback only for codecs
   measured cheap (H.264). Discarded readers → retirement pool (HEVC drop ≈ 1 s).
2. Queue budget arithmetic should assume **fitted-size frames** (2×4K RGBA ≈ 64 MiB budget
   holds ~4 × 1440p frames — still capped at 3 by the frame bound).
3. Producer thread owns the BGRX→RGBA copy; it is the hot per-frame cost (13 ms @4K), so it
   must never migrate to the event loop or the present path.
4. Schedule: **the Windows milestone estimate stands (2–3 wk)** — spikes surfaced no
   unknown-unknowns; the HEVC teardown/seek costs were already architected for (retirement
   pool, fresh-reader seeks).

## Still open in phase 0 (not Windows-blocking)

- macOS: AVAssetReader recreate cost + `prepareToPlay()` timing (needs the Mac).
- Linux: incremental PCM audio prototype (current path pre-decodes whole-clip audio).
- Clean-VM (no Store extensions) sweep to write the out-of-box row of the format matrix.
- Real HLG phone clip for the corpus.
