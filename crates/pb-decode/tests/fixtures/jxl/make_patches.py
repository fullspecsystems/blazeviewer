#!/usr/bin/env python3
"""Write a synthetic colour card (task #156) as an 8-bit PNG of flat patches, tagged with a
`cICP` chunk so cjxl records the colour space as an enum encoding.

The patches are code values in the card's *own* space: tagged Display P3, patch 0 is P3 red
(255, 0, 0), well outside sRGB, which is what a lossy (XYB) JPEG XL must survive. Keep the
list in step with `PATCHES` in crates/pb-decode/src/jxl.rs.

Standard library only, so the card is independent of every decoder under test.

    make_patches.py p3   out.png          # Display P3 (cICP 12/13/0/1), RGB
    make_patches.py p3a  out.png          # the same, RGBA with alpha 200
    make_patches.py srgb out.png          # sRGB (cICP 1/13/0/1), RGB
    make_patches.py p3-16 out.png         # Display P3, 16-bit RGB (each code times 257)
"""
import struct
import sys
import zlib

PATCHES = [
    (255, 0, 0),  # red: outside sRGB when the card is P3
    (0, 255, 0),  # green: outside sRGB when the card is P3
    (0, 0, 255),  # blue
    (255, 255, 255),  # white
    (128, 128, 128),  # mid grey
    (230, 140, 60),  # an orange inside sRGB either way
]
PATCH = 32  # px; a whole number of 8x8 coding blocks
ALPHA = 200


def chunk(kind, data):
    body = kind + data
    return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))


def main():
    space, out = sys.argv[1], sys.argv[2]
    primaries, alpha, depth = {
        "p3": (12, False, 8),
        "p3a": (12, True, 8),
        "srgb": (1, False, 8),
        "p3-16": (12, False, 16),
    }[space]
    width, height = PATCH * len(PATCHES), PATCH
    if depth == 16:
        px = lambda c: struct.pack(">HHH", *(v * 257 for v in c))
    elif alpha:
        px = lambda c: bytes(c) + bytes([ALPHA])
    else:
        px = bytes
    row = b"".join(px(c) * PATCH for c in PATCHES)
    raw = b"".join(b"\x00" + row for _ in range(height))
    png = b"\x89PNG\r\n\x1a\n"
    colour_type = 6 if alpha else 2
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, depth, colour_type, 0, 0, 0))
    # cICP: the primaries, transfer 13 (sRGB), matrix 0 (RGB), full range.
    png += chunk(b"cICP", bytes([primaries, 13, 0, 1]))
    png += chunk(b"IDAT", zlib.compress(raw, 9))
    png += chunk(b"IEND", b"")
    with open(out, "wb") as f:
        f.write(png)


if __name__ == "__main__":
    main()
