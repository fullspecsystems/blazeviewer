#!/usr/bin/env bash
# Re-copy pb-jpegli's vendored sources (see README.md in this folder).
#
#   refresh.sh <libjxl checkout> <libjpeg-turbo checkout> <highway checkout>
#
# Each checkout must already be at the commit you want. jconfig.h is hand-written and is left
# alone. Run `cargo test -p pb-jpegli` afterwards: if jpegli gained a source file or an include,
# the build says so.
set -euo pipefail

if [ $# -ne 3 ]; then
  echo "usage: $0 <libjxl> <libjpeg-turbo> <highway>" >&2
  exit 2
fi
LIBJXL=$1
LJT=$2
HWY=$3
HERE=$(cd "$(dirname "$0")" && pwd)

# jpegli's encoder half: the files build.rs compiles and the headers they include.
JPEGLI_FILES="
adaptive_quantization.cc adaptive_quantization.h bit_writer.cc bit_writer.h bitstream.cc
bitstream.h color_transform.cc color_transform.h common.cc common.h common_internal.h dct-inl.h
decode_internal.h destination_manager.cc downsample.cc downsample.h encode.cc encode.h
encode_finish.cc encode_finish.h encode_internal.h encode_streaming.cc encode_streaming.h
entropy_coding-inl.h entropy_coding.cc entropy_coding.h error.cc error.h huffman.cc huffman.h
input.cc input.h memory_manager.cc memory_manager.h quant.cc quant.h simd.cc simd.h
transpose-inl.h types.h
"
JXL_BASE_FILES="bits.h byte_order.h common.h compiler_specific.h sanitizer_definitions.h status.h"

rm -rf "$HERE/libjxl" "$HERE/highway"
find "$HERE/libjpeg-turbo" -type f ! -name jconfig.h -delete

mkdir -p "$HERE/libjxl/lib/jpegli" "$HERE/libjxl/lib/jxl/base" "$HERE/libjxl/lib/include/jxl"
for f in $JPEGLI_FILES; do cp "$LIBJXL/lib/jpegli/$f" "$HERE/libjxl/lib/jpegli/"; done
for f in $JXL_BASE_FILES; do cp "$LIBJXL/lib/jxl/base/$f" "$HERE/libjxl/lib/jxl/base/"; done
cp "$LIBJXL/lib/include/jxl/types.h" "$HERE/libjxl/lib/include/jxl/"
cp "$LIBJXL/LICENSE" "$LIBJXL/PATENTS" "$LIBJXL/AUTHORS" "$HERE/libjxl/"

for f in jpeglib.h jmorecfg.h jerror.h LICENSE.md README.ijg; do cp "$LJT/$f" "$HERE/libjpeg-turbo/"; done

(cd "$HWY" && find hwy -name '*.h' -not -path '*/tests/*' -not -path '*/contrib/*' \
  -not -path '*/examples/*') | while read -r f; do
  mkdir -p "$HERE/highway/$(dirname "$f")"
  cp "$HWY/$f" "$HERE/highway/$f"
done
cp "$HWY/LICENSE" "$HWY/LICENSE-BSD3" "$HERE/highway/"

echo "refreshed; now check build.rs's HWY_VERSION and run cargo test -p pb-jpegli"
