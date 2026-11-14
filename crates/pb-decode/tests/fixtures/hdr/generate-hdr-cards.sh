#!/usr/bin/env bash
# Regenerate the synthetic HDR test cards for the brightness audit (task #146,
# crates/pb-decode/src/hdr_audit.rs). Flat grey patches of known nits, so they are safe in a
# public repo. Every encoder here is independent of the decoders under test.
#
# Needs: python3, avifenc (libavif), heif-enc (libheif + x265), cjxl (libjxl), ffmpeg (libx265).
#   brew install libavif libheif jpeg-xl ffmpeg
#
# Run from anywhere: writes next to this script.
set -euo pipefail
cd "$(dirname "$0")"

for spec in pq:16:smpte2084 hlg:18:arib-std-b67; do
  IFS=: read -r name tc trc <<<"$spec"

  # The source: a 16-bit RGB PNG with exact code values + a cICP chunk (BT.2020, PQ/HLG).
  python3 make_card.py "$name" "$name-card.png"

  # AVIF, 10-bit 4:4:4, full range, BT.2020 NCL — the shape Image I/O / WIC decode.
  avifenc -d 10 -y 444 --cicp "9/$tc/9" -r full -q 100 --ignore-icc \
    "$name-card.png" "$name-card.avif" >/dev/null

  # HEIC, 10-bit 4:4:4 (x265), same code points.
  heif-enc -b 10 --matrix_coefficients 9 --colour_primaries 9 \
    --transfer_characteristic "$tc" --full_range_flag 1 -p chroma=444 -q 95 \
    "$name-card.png" -o "$name-card.heic" >/dev/null

  # JPEG XL, lossless; cjxl takes the colour encoding from the PNG's cICP chunk.
  cjxl "$name-card.png" "$name-card.jxl" -d 0 >/dev/null 2>&1

  # A 3-frame HEVC clip, 10-bit 4:2:0 limited range, tagged in BOTH the bitstream (x265 VUI)
  # and the container (colr). FFmpeg trusts the VUI and AVFoundation the container, so a clip
  # tagged in only one place reads as SDR on one route.
  ffmpeg -hide_banner -loglevel error -y -loop 1 -i "$name-card.png" -frames:v 3 -r 30 \
    -vf "scale=out_color_matrix=bt2020nc:out_range=tv,format=yuv420p10le" \
    -c:v libx265 \
    -x265-params "log-level=error:crf=4:colorprim=bt2020:transfer=$trc:colormatrix=bt2020nc:range=limited" \
    -color_primaries bt2020 -color_trc "$trc" -colorspace bt2020nc -color_range tv \
    -tag:v hvc1 "$name-card.mp4"
  # Fail loudly if the VUI lost the transfer (it has happened — see above).
  got=$(ffprobe -v error -select_streams v:0 -read_intervals %+#1 \
    -show_entries frame=color_transfer -of csv=p=0 "$name-card.mp4" | head -1 | tr -d ',')
  [[ "$got" == "$trc" ]] || { echo "$name-card.mp4: VUI transfer is '$got', want $trc" >&2; exit 1; }
done
ls -la ./*-card.*
