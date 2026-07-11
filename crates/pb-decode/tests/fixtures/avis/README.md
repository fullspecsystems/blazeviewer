# avis (animated AVIF) decode fixtures — task #76

Tiny real `avis` files (~1 KB each, libaom via ffmpeg 7.1) with **per-frame
solid colors**, so the decode-integration tests (`src/avis/tests.rs`,
`decode_integration`) can assert exact mean RGB with a small codec tolerance —
no reference decoder needed in CI.

Note `lime` (#00FF00), not `green` (#008000 — the half-bright HTML green).
`setparams` stamps CICP color properties frame-level; plain `-color_primaries`
output flags get lost through `-filter_complex` and land as "unspecified".

Regeneration (PowerShell; `$enc = "-c:v","libaom-av1","-crf","20","-b:v","0","-cpu-used","8"`;
solid PNGs made with `ffmpeg -f lavfi -i color=<name>:size=64x64 -frames:v 1 <name>.png`):

| Fixture | Purpose | Command core |
|---|---|---|
| `rgb_8bit_420.avif` | 3 frames R/lime/B, 8 fps | `-framerate 8 -i red.png … -filter_complex "[0][1][2]concat=n=3:v=1:a=0" -r 8 $enc -pix_fmt yuv420p` |
| `rgb_10bit_420.avif` | 10-bit depth path | same, `-pix_fmt yuv420p10le` |
| `rgb_8bit_444.avif` | 4:4:4 layout | same (2 frames), `-pix_fmt yuv444p` |
| `odd_63x37.avif` | odd dims / ceil chroma | `…concat…,scale=63:37` |
| `alpha_64x64.avif` | aux alpha track; color-track selection; opaque v1 output | inputs `color=red@0.5…,format=rgba`, `-pix_fmt yuva420p` |
| `vardur_64x64.avif` | variable `stts` durations | concat **demuxer** with per-entry `duration` lines + `-vsync vfr` |
| `p3_64x64.avif` | non-sRGB `ColorTransform` carried | `…concat…,setparams=color_primaries=smpte432:color_trc=iec61966-2-1:colorspace=bt709` |
| `hdr_pq_64x64.avif` | PQ → probe **rejects** (still path) | `…concat…,setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc`, `-pix_fmt yuv420p10le` |
