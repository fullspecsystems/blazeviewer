# Benchmark & Test Image Corpus

A **reproducible, checksummed** set of images is the backbone of every
performance claim and golden-image test in PhotoBlaze. Without a fixed corpus,
A/B numbers are noise.

## Rules

1. **Pinned & checksummed.** Every file is tracked (Git LFS once the repo is
   initialized) and listed with its sha256 in `manifest.tsv`. Benchmarks refuse
   to run if a checksum mismatches.
2. **Decoded from memory in benches.** Read the bytes once, then time *decode
   only* — never let disk I/O leak into decode microbenchmarks.
3. **Spans the real workload.** Cover the formats and the size/feature axes that
   actually change decode cost.

## Coverage matrix (target)

| Axis | Buckets |
|------|---------|
| Format | jpeg, png, webp, avif, heic, jxl, tiff, bmp, qoi, svg, raw(+embedded preview) |
| Megapixels | ~2 MP, ~12 MP, ~24 MP, ~60 MP |
| Aspect | landscape (16:9), portrait (2:3), square, panorama (≥3:1) |
| Color | sRGB, Display-P3, Adobe RGB, ICC-tagged, untagged |
| Bit depth | 8-bit, ≥10-bit (HDR candidates) |
| JPEG subsampling | 4:2:0, 4:2:2, 4:4:4 (DCT-scaled-decode behavior differs) |
| Pathological | progressive JPEG, CMYK JPEG, huge thumbnail, truncated/corrupt (fuzz/robustness) |

## Files

- `manifest.tsv` — `relpath \t sha256 \t format \t width \t height \t notes`
- Real photos that can't be redistributed stay out of git; the manifest records
  their checksums so a local corpus can be validated without shipping the bytes.

> Licensing note: prefer CC0 / public-domain sources (e.g. format-conformance
> suites, openverse CC0) so the corpus can live in the repo. Anything else is
> referenced by checksum only.
