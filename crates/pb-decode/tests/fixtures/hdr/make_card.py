#!/usr/bin/env python3
"""Write a synthetic HDR test card (task #146) as a 16-bit RGB PNG of flat grey patches.

Each patch is a known display brightness in nits, encoded with the exact PQ (SMPTE ST 2084)
or HLG (ARIB STD-B67, BT.2100 1000-nit OOTF) code value, and the PNG carries a `cICP` chunk
(BT.2020 primaries, the transfer, RGB, full range) so encoders that read it tag correctly.
The decoders under test must turn patch *k* back into `nits[k] / 203` (1.0 = 203-nit SDR
reference white, BT.2408) — see `hdr_audit.rs`.

Standard library only, so the card is independent of every decoder we test.

    make_card.py pq  out.png
    make_card.py hlg out.png
"""
import math
import struct
import sys
import zlib

# Keep in step with `PQ_NITS` / `HLG_NITS` in crates/pb-decode/src/hdr_audit.rs.
PQ_NITS = [0, 100, 203, 400, 1000, 4000]
HLG_NITS = [0, 50, 100, 203, 400, 1000]
PATCH = 64  # px; even, and a whole number of 8x8 / 16x16 coding blocks


def pq_encode(nits):
    """Inverse PQ EOTF: absolute luminance → code value in [0, 1]."""
    m1, m2 = 2610 / 16384, 2523 / 4096 * 128
    c1, c2, c3 = 3424 / 4096, 2413 / 4096 * 32, 2392 / 4096 * 32
    y = (nits / 10000) ** m1
    return ((c1 + c2 * y) / (1 + c3 * y)) ** m2


def hlg_encode(nits):
    """Inverse of the 1000-nit BT.2100 OOTF (gamma 1.2, black 0), then the HLG OETF.
    For a grey patch the luminance OOTF and the per-channel form agree."""
    a, b, c = 0.17883277, 0.28466892, 0.55991073
    scene = (nits / 1000) ** (1 / 1.2)
    if scene <= 1 / 12:
        return math.sqrt(3 * scene)
    return a * math.log(12 * scene - b) + c


def chunk(kind, data):
    body = kind + data
    return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))


def main():
    transfer, out = sys.argv[1], sys.argv[2]
    nits, encode, tc = {
        "pq": (PQ_NITS, pq_encode, 16),
        "hlg": (HLG_NITS, hlg_encode, 18),
    }[transfer]
    codes = [round(encode(n) * 65535) for n in nits]
    width, height = PATCH * len(nits), PATCH
    row = b"".join(struct.pack(">HHH", v, v, v) * PATCH for v in codes)
    raw = b"".join(b"\x00" + row for _ in range(height))
    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 16, 2, 0, 0, 0))
    # cICP: primaries 9 (BT.2020), transfer 16 (PQ) / 18 (HLG), matrix 0 (RGB), full range.
    png += chunk(b"cICP", bytes([9, tc, 0, 1]))
    png += chunk(b"IDAT", zlib.compress(raw, 9))
    png += chunk(b"IEND", b"")
    with open(out, "wb") as f:
        f.write(png)
    for n, v in zip(nits, codes):
        print(f"{transfer} {n:>5} nits -> code {v / 65535:.4f}")


if __name__ == "__main__":
    main()
