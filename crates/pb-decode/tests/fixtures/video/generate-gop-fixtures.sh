#!/usr/bin/env bash
# Generate the seek-robustness GOP fixtures (plan §T3), checked in because they are tiny and
# because CI and a fresh clone have no /Volumes/Media. Two H.264-in-MKV clips that differ only
# in GOP length:
#
#   longgop.mkv  — 5 s GOP (keyframes at 0/5/10/…/25 s). The H1 pre-roll-starvation repro: a
#                  2 s hop inside a 5 s GOP decodes a long DoNotDisplay pre-roll.
#   shortgop.mkv — 0.5 s GOP. The control; must pass before AND after any seek fix.
#
# Both carry a 440 Hz tone so audio-seek coordination (H2') has something to hear. Determinism:
# the bytes are fixed once committed; this script only regenerates them. Re-run it if the
# fixtures are ever lost — the exact ffmpeg build may shift the bytes, which is fine (tests
# assert timestamps/keyframes, not a byte hash).
#
# ⚠ The plan wants a burned-in timecode (drawtext) so the Swift *visual* harness (T2) can
# assert a seek to T against the picture. This box's ffmpeg has no drawtext, so these are
# generated WITHOUT it — enough for the Rust demux seek contract (T4, packet timestamps) and
# the audio work, but the T2 visual harness must regenerate them on a drawtext-capable ffmpeg
# (the `--enable-libfreetype` build). The block below adds it automatically when present.
set -euo pipefail
cd "$(dirname "$0")"

DUR=30
RATE=24

vf() { # emit a -vf drawtext timecode arg iff this ffmpeg has drawtext, else nothing
	if ffmpeg -hide_banner -filters 2>/dev/null | grep -qw drawtext; then
		printf -- '-vf drawtext=text=%%{pts\\:hms}:fontcolor=white:fontsize=36:x=10:y=10'
	fi
}

gen() { # gen <out> <gop>
	local out=$1 gop=$2
	# shellcheck disable=SC2046
	ffmpeg -y -hide_banner -loglevel error \
		-f lavfi -i "testsrc=size=320x240:rate=$RATE:duration=$DUR" \
		-f lavfi -i "sine=frequency=440:duration=$DUR" \
		$(vf) \
		-c:v libx264 -g "$gop" -keyint_min "$gop" -sc_threshold 0 -pix_fmt yuv420p -crf 34 \
		-c:a aac -b:a 32k -shortest "$out"
	echo "wrote $out ($(du -h "$out" | cut -f1))"
}

gen longgop.mkv 120  # 120 frames / 24 fps = 5 s GOP
gen shortgop.mkv 12  # 12 frames / 24 fps = 0.5 s GOP
