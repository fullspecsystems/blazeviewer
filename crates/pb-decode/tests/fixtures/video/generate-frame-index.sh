#!/usr/bin/env bash
# Generate frame_index.mp4 for the frame-capture tests (task #81, pb-decode
# ffmpeg/frame_at.rs): 30 frames at 30 fps, 64x48, frame k a flat grey of RGB 16 + 8k —
# so the frame a capture returns is identifiable by its brightness. H.264 with 3 B-frames and
# ONE keyframe (at 0), so reaching frame 29 means decoding forward through reordered frames.
# Tagged BT.709 limited range in both the bitstream and the container.
set -euo pipefail
cd "$(dirname "$0")"
ffmpeg -y -hide_banner -loglevel error \
	-f lavfi -i "color=c=black:size=64x48:rate=30:duration=1" \
	-vf "format=rgb24,geq=r='16+8*N':g='16+8*N':b='16+8*N',scale=out_color_matrix=bt709:out_range=tv,format=yuv420p" \
	-c:v libx264 -g 300 -keyint_min 300 -sc_threshold 0 -crf 4 \
	-x264-params "bframes=3:b-adapt=0:colorprim=bt709:transfer=bt709:colormatrix=bt709" \
	-color_primaries bt709 -color_trc bt709 -colorspace bt709 -color_range tv \
	-frames:v 30 frame_index.mp4
ffprobe -v error -select_streams v:0 -count_frames \
	-show_entries stream=nb_read_frames,has_b_frames,codec_name -of compact=p=0 frame_index.mp4
