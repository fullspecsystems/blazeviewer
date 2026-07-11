# Video test fixtures (task #79)

Tiny committed clips for the poster/metadata probes (`mf_poster.rs` tests). H.264 only —
in the box on every supported Windows; codec-extension formats (HEVC/AV1/VP9) are
exercised by the opt-in corpus, never by committed fixtures.

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
