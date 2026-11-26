#!/usr/bin/env bash
# Regenerate the JPEG XL colour cards for task #156 (crates/pb-decode/src/jxl.rs tests):
# flat patches, so they are safe in a public repo. cjxl is libjxl, independent of jxl-oxide,
# the decoder under test.
#
# Needs: python3, cjxl (libjxl).   brew install jpeg-xl
#
# Run from anywhere: writes next to this script. The PNGs are intermediates and are removed.
set -euo pipefail
cd "$(dirname "$0")"

python3 make_patches.py p3 p3-card.png
python3 make_patches.py p3a p3a-card.png
python3 make_patches.py srgb srgb-card.png
python3 make_patches.py p3-16 p3-16-card.png

# Lossy is XYB (VarDCT): the case jxl-oxide squeezes through sRGB. Lossless keeps the
# original space and never converts, so it is the control.
cjxl p3-card.png p3-lossy.jxl -d 1 >/dev/null 2>&1
cjxl p3a-card.png p3-alpha-lossy.jxl -d 1 >/dev/null 2>&1
cjxl p3-card.png p3-lossless.jxl -d 0 >/dev/null 2>&1
cjxl srgb-card.png srgb-lossy.jxl -d 1 >/dev/null 2>&1
# 16-bit lossy: the export decode keeps 16 bits, so its re-encode has a second precision.
cjxl p3-16-card.png p3-16-lossy.jxl -d 1 >/dev/null 2>&1

rm -f ./*-card.png
ls -la ./*.jxl
