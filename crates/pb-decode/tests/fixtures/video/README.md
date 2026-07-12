# Video test fixtures (tasks #79, #84)

Tiny committed clips for the poster/metadata probes (`mf_poster.rs` tests) and the
FFmpeg producer/poster (`ffmpeg/` tests, `--features ffvideo`). All lavfi-generated
(no provenance/licensing exposure). The **Windows (MF) tests use H.264 only** — in
the box on every supported Windows; codec-extension formats (HEVC/AV1) are exercised
by the opt-in corpus there. The **FFmpeg tests also use the VP8/VP9/MKV fixtures
below** — those decoders are built into every FFmpeg, so committing them is safe.

## black_then_color.mp4

64×64 @ 30 fps, ~1 s, H.264 yuv420p, no audio: ~12 black frames then solid orange —
exercises the first-non-black mean-luma walk (the poster must skip the lead-in).

Regen:

```sh
ffmpeg -y -f lavfi -i "color=black:size=64x64:rate=30:duration=0.4" \
       -f lavfi -i "color=orange:size=64x64:rate=30:duration=0.6" \
       -filter_complex "[0:v][1:v]concat=n=2:v=1:a=0" \
       -c:v libx264 -pix_fmt yuv420p -crf 30 black_then_color.mp4
```

## color_with_tone.mp4

64×64 @ 30 fps, 1 s, H.264 + a 440 Hz AAC tone — exercises `has_audio` detection
(the producer's `Opened` event, phase 5's start-the-audio-player signal).

Regen:

```sh
ffmpeg -y -f lavfi -i "color=orange:size=64x64:rate=30:duration=1" \
       -f lavfi -i "sine=frequency=440:duration=1" \
       -c:v libx264 -pix_fmt yuv420p -crf 30 -c:a aac -b:a 32k -shortest color_with_tone.mp4
```

## color_vp9.webm / color_vp8.webm (task #84)

64×64 @ 30 fps, 1 s, solid orange, VP9 / VP8 in WebM, no audio — the macOS-fallback
flagship formats (AVFoundation can't demux WebM or decode VP8/VP9) through the FFmpeg
producer end-to-end.

Regen:

```sh
ffmpeg -y -f lavfi -i "color=orange:size=64x64:rate=30:duration=1" \
       -c:v libvpx-vp9 -pix_fmt yuv420p -b:v 50k color_vp9.webm
ffmpeg -y -f lavfi -i "color=orange:size=64x64:rate=30:duration=1" \
       -c:v libvpx -pix_fmt yuv420p -b:v 50k color_vp8.webm
```

## black_then_color.mkv (task #84)

The `black_then_color.mp4` content re-muxed as H.264-in-Matroska — the "MKV commonly
wraps H.264" case AVFoundation can't demux. Same poster-walk contract as the mp4.

Regen: the `black_then_color.mp4` command with an `.mkv` output name.

## rotated90.mp4 (task #84)

64×32 @ 30 fps, 1 s, orange, H.264 with a 90° display-matrix — the FFmpeg
producer/poster must emit upright 32×64 frames (rotation applied post-scale).

Regen:

```sh
ffmpeg -y -f lavfi -i "color=orange:size=64x32:rate=30:duration=1" \
       -c:v libx264 -pix_fmt yuv420p -crf 30 /tmp/src.mp4
ffmpeg -y -display_rotation 90 -i /tmp/src.mp4 -c copy rotated90.mp4
```

## hdr_pq.mp4 (task #84)

64×64 @ 30 fps, 1 s, orange, HEVC 10-bit PQ (smpte2084 / bt2020 / bt2020nc) — the
FFmpeg fp16 HDR path (plan §9): frames must leave as scene-linear scRGB Rgba16F,
never tone-mapped RGBA8.

Regen:

```sh
ffmpeg -y -f lavfi -i "color=orange:size=64x64:rate=30:duration=1" \
       -c:v libx265 -pix_fmt yuv420p10le \
       -x265-params "colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc" \
       -tag:v hvc1 hdr_pq.mp4
```
