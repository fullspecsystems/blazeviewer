//! Photoshop `.psd` backend: the pre-flattened composite, decoded in-crate.
//!
//! A viewer only needs the *merged* image, not a layer compositor. Every PSD ends
//! with an "image data" section holding exactly that merged composite, and its
//! format is small and fully specified — a header, three length-prefixed sections
//! we skip, then planar channel data that's either raw or PackBits/RLE-compressed.
//! So we decode it ourselves. That's deliberate: the two third-party PSD crates we
//! tried each failed on real files — `psd` *panicked* on a majority of them (it
//! eagerly parses layer/resource blocks it mishandles), and `zune-psd` returns
//! "not implemented" for the very common **16-bit RLE** case (and mis-deplanarizes
//! uncompressed 8-bit). Owning the ~1-page composite reader sidesteps both and has
//! no gaps for the RGB/Gray files a photographer actually views.
//!
//! Coverage: color modes **Grayscale** and **RGB** (with or without alpha),
//! **8- and 16-bit** depth (16-bit is narrowed to 8 for the RGBA8 texture — PSD
//! isn't HDR), **raw or RLE** compression. Other modes (CMYK / Lab / Indexed /
//! Duotone / Multichannel) and the rare ZIP-compressed composite decode-error
//! cleanly, so the viewer skips them rather than showing wrong colors. The result
//! runs through shared [`common::finalize_oriented`] decode-to-fit like every other
//! backend, so a PSD is perf-neutral: one decode, held resident in the ring.
//!
//! Routed by the `8BPS` magic ([`ImageDecoder::can_decode`] returns true), so a
//! mislabeled name still lands here. PSD composites are already upright and PSD
//! carries no EXIF orientation, so no orientation read is needed. `.psb` (Large
//! Document Format, version 2) is a distinct variant and is not advertised (see
//! `is_supported_extension`).

use crate::{common, DecodeError, DecodeRequest, DecodedImage, ImageDecoder};

/// Photoshop document signature ("8BPS"), shared by `.psd` and `.psb`.
const PSD_MAGIC: &[u8] = b"8BPS";

/// PSD file-header size (fixed): signature, version, 6 reserved, channels, height,
/// width, depth, color mode — then the four length-prefixed sections.
const HEADER_LEN: usize = 26;

/// PSD color modes we render (samples are already RGB/Gray, no conversion needed).
const MODE_GRAYSCALE: u16 = 1;
const MODE_RGB: u16 = 3;

/// Image-data compression markers.
const COMPRESSION_RAW: u16 = 0;
const COMPRESSION_RLE: u16 = 1;

#[derive(Debug, Clone, Copy, Default)]
pub struct PsdDecoder;

impl ImageDecoder for PsdDecoder {
    fn can_decode(&self, bytes: &[u8]) -> bool {
        bytes.starts_with(PSD_MAGIC)
    }

    fn decode(&self, req: &DecodeRequest) -> Result<DecodedImage, DecodeError> {
        let (rgba, w, h) = decode_composite(req.bytes)?;
        // Orientation 1: PSD composites are upright and the format carries no EXIF
        // orientation. `finalize_oriented` then decode-to-fit downscales to `fit`.
        common::finalize_oriented(rgba, w, h, 1, "PSD", req.fit, false)
    }

    fn name(&self) -> &'static str {
        "psd"
    }
}

/// The PSD file header we route on: dimensions, channel count, depth, color mode.
struct PsdHeader {
    channels: usize,
    height: usize,
    width: usize,
    depth: u16,
    mode: u16,
}

fn be_u16(b: &[u8], off: usize) -> Option<u16> {
    b.get(off..off + 2)
        .map(|s| u16::from_be_bytes([s[0], s[1]]))
}

fn be_u32(b: &[u8], off: usize) -> Option<u32> {
    b.get(off..off + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

/// Parse the fixed 26-byte header. `None` if it isn't a version-1 PSD (version 2 is
/// `.psb`, which we don't advertise).
fn parse_header(bytes: &[u8]) -> Option<PsdHeader> {
    if !bytes.starts_with(PSD_MAGIC) || be_u16(bytes, 4)? != 1 {
        return None;
    }
    Some(PsdHeader {
        channels: be_u16(bytes, 12)? as usize,
        height: be_u32(bytes, 14)? as usize,
        width: be_u32(bytes, 18)? as usize,
        depth: be_u16(bytes, 22)?,
        mode: be_u16(bytes, 24)?,
    })
}

/// Byte offset of the image-data section's compression marker: walk the three
/// length-prefixed sections (color-mode data, image resources, layer & mask info)
/// after the header, each a big-endian `u32` length + payload. Bounds-checked, so a
/// truncated/hostile file yields `None` rather than panicking.
fn image_data_offset(bytes: &[u8]) -> Option<usize> {
    let mut off = HEADER_LEN;
    for _ in 0..3 {
        let len = be_u32(bytes, off)? as usize;
        off = off.checked_add(4)?.checked_add(len)?;
    }
    (off + 2 <= bytes.len()).then_some(off)
}

/// The "Version Info" image resource (ID 1057).
const RESOURCE_VERSION_INFO: u16 = 1057;

/// Whether the file's Version-Info resource (1057) declares the merged composite a
/// **placeholder** (`hasRealMergedData == 0`) — the authoritative "saved without
/// Maximize Compatibility" signal. When true, the image-data section is a blank
/// stand-in (a solid white frame; the real pixels live only in the layers, which a
/// viewer doesn't composite), so we skip rather than show it. Absent/unparseable →
/// `false` (assume a real composite; never wrongly reject a valid file — a genuinely
/// solid-color PSD keeps `hasRealMergedData == 1`, so this can't false-positive it).
///
/// Image resources are a run of blocks: `8BIM`, a `u16` id, a padded-even Pascal
/// name, a `u32` data size, then padded-even data. Bounds-checked throughout.
fn composite_is_placeholder(bytes: &[u8]) -> bool {
    // Image resources are the *second* section: skip the color-mode-data section
    // (its `u32` length at the header) to reach the resources length + payload.
    let Some(cm_len) = be_u32(bytes, HEADER_LEN) else {
        return false;
    };
    let ir_len_off = HEADER_LEN + 4 + cm_len as usize;
    let Some(len) = be_u32(bytes, ir_len_off) else {
        return false;
    };
    let start = ir_len_off + 4;
    let end = start.saturating_add(len as usize).min(bytes.len());
    let mut p = start;
    while p + 6 <= end {
        if &bytes[p..p + 4] != b"8BIM" {
            break;
        }
        let id = match be_u16(bytes, p + 4) {
            Some(v) => v,
            None => break,
        };
        p += 6;
        // Pascal name: 1 length byte + name, padded so the (len+name) span is even.
        let name_span = 1 + *bytes.get(p).unwrap_or(&0) as usize;
        p += name_span + (name_span & 1);
        let size = match be_u32(bytes, p) {
            Some(v) => v as usize,
            None => break,
        };
        p += 4;
        if id == RESOURCE_VERSION_INFO {
            // Layout: version (u32), then the hasRealMergedData byte. 0 = placeholder.
            return bytes.get(p + 4) == Some(&0);
        }
        p = match p.checked_add(size + (size & 1)) {
            Some(v) => v,
            None => break,
        };
    }
    false
}

/// Decode the merged composite to straight RGBA8, returning `(rgba, width, height)`.
///
/// The image-data section is **planar**: `channels` consecutive planes (Red, Green,
/// Blue, [Alpha], …), each `width*height*bytes_per_sample` bytes, either stored raw
/// or PackBits/RLE-compressed. We deplanarize, narrow 16-bit samples to 8-bit, and
/// expand Gray/GrayA/RGB/RGBA to RGBA8.
fn decode_composite(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), DecodeError> {
    let hdr = parse_header(bytes)
        .ok_or_else(|| DecodeError::Corrupt("psd: not a version-1 PSD".into()))?;

    // Files saved without "Maximize Compatibility" carry only a blank white
    // placeholder here (the real image is in the layers, which we don't composite).
    // Skip cleanly rather than show a white frame.
    if composite_is_placeholder(bytes) {
        return Err(DecodeError::Corrupt(
            "psd: no embedded composite (saved without Maximize Compatibility)".into(),
        ));
    }

    // Samples are only directly renderable for Grayscale / RGB. Other modes (CMYK,
    // Lab, Indexed, Duotone, Multichannel) would need conversion we don't do.
    if !matches!(hdr.mode, MODE_GRAYSCALE | MODE_RGB) {
        return Err(DecodeError::Corrupt(format!(
            "psd: unsupported color mode {} (only Grayscale/RGB)",
            hdr.mode
        )));
    }
    let bps: usize = match hdr.depth {
        8 => 1,
        16 => 2,
        // 1-bit bitmap and 32-bit float are rare and need their own unpacking.
        other => {
            return Err(DecodeError::Corrupt(format!(
                "psd: unsupported depth {other}"
            )))
        }
    };
    let px = hdr
        .width
        .checked_mul(hdr.height)
        .filter(|&n| n > 0)
        .ok_or_else(|| {
            DecodeError::Corrupt(format!("psd: bad geometry {}x{}", hdr.width, hdr.height))
        })?;
    // Guard against a header whose channel count disagrees with its color mode.
    let max_channels = if hdr.mode == MODE_GRAYSCALE { 2 } else { 4 };
    if hdr.channels == 0 || hdr.channels > max_channels {
        return Err(DecodeError::Corrupt(format!(
            "psd: {} channels invalid for mode {}",
            hdr.channels, hdr.mode
        )));
    }

    let off = image_data_offset(bytes)
        .ok_or_else(|| DecodeError::Corrupt("psd: image-data section truncated".into()))?;
    let compression = be_u16(bytes, off).unwrap_or(u16::MAX);
    let body = &bytes[off + 2..];

    let plane_bytes = px * bps; // one channel plane, decompressed
    let planar: Vec<u8> = match compression {
        COMPRESSION_RAW => {
            let need = plane_bytes * hdr.channels;
            body.get(..need)
                .ok_or_else(|| DecodeError::Corrupt("psd: raw composite truncated".into()))?
                .to_vec()
        }
        COMPRESSION_RLE => rle_planar(body, hdr.channels, hdr.height, hdr.width * bps)?,
        other => {
            // ZIP (2/3) is used for layer channels, effectively never the merged
            // composite — treat as a clean skip rather than guessing.
            return Err(DecodeError::Corrupt(format!(
                "psd: unsupported compression {other}"
            )));
        }
    };

    Ok((
        planar_to_rgba8(&planar, px, hdr.channels, bps),
        hdr.width as u32,
        hdr.height as u32,
    ))
}

/// Decompress a PackBits/RLE image-data body to `channels*height` planar rows of
/// `row_len` bytes each. The RLE body opens with a `channels*height` table of
/// big-endian `u16` per-row byte counts; each row's compressed span is then
/// PackBits-decoded to exactly `row_len` bytes.
fn rle_planar(
    body: &[u8],
    channels: usize,
    height: usize,
    row_len: usize,
) -> Result<Vec<u8>, DecodeError> {
    let corrupt = |m: &str| DecodeError::Corrupt(format!("psd: {m}"));
    let n_rows = channels * height;
    let table_len = n_rows * 2;
    let counts = body
        .get(..table_len)
        .ok_or_else(|| corrupt("RLE row-count table truncated"))?;
    let mut comp = &body[table_len..];

    let mut out = Vec::with_capacity(n_rows * row_len);
    for r in 0..n_rows {
        let count = be_u16(counts, r * 2).unwrap() as usize;
        let row = comp
            .get(..count)
            .ok_or_else(|| corrupt("RLE row data truncated"))?;
        unpack_bits_into(row, row_len, &mut out).ok_or_else(|| corrupt("RLE row malformed"))?;
        comp = &comp[count..];
    }
    Ok(out)
}

/// PackBits (Apple/TIFF/PSD RLE) decode of one row, appending exactly `expected`
/// bytes to `out`. Returns `None` on a malformed run or a length mismatch.
fn unpack_bits_into(src: &[u8], expected: usize, out: &mut Vec<u8>) -> Option<()> {
    let start = out.len();
    let mut i = 0;
    while out.len() - start < expected {
        let n = *src.get(i)? as i8;
        i += 1;
        if n >= 0 {
            // Copy the next n+1 bytes literally.
            let count = n as usize + 1;
            let chunk = src.get(i..i + count)?;
            out.extend_from_slice(chunk);
            i += count;
        } else if n != -128 {
            // Repeat the next byte (1 - n) times (−128 is a no-op).
            let count = (1 - n as isize) as usize;
            let b = *src.get(i)?;
            i += 1;
            out.extend(std::iter::repeat_n(b, count));
        }
    }
    (out.len() - start == expected).then_some(())
}

/// Deplanarize `channels` planes into straight RGBA8. 16-bit samples (`bps == 2`,
/// big-endian) are narrowed to 8-bit by taking the high byte. Channel layouts:
/// 1 → Luma, 2 → Luma+Alpha, 3 → RGB, 4 → RGBA.
fn planar_to_rgba8(planar: &[u8], px: usize, channels: usize, bps: usize) -> Vec<u8> {
    let plane = px * bps; // stride from one channel's plane to the next
                          // High byte of sample `i` in channel `c` (BE for 16-bit; the byte itself for 8-bit).
    let sample = |c: usize, i: usize| planar[c * plane + i * bps];

    let mut out = vec![0u8; px * 4];
    for (i, dst) in out.chunks_exact_mut(4).enumerate() {
        match channels {
            1 => {
                let y = sample(0, i);
                dst.copy_from_slice(&[y, y, y, 255]);
            }
            2 => {
                let y = sample(0, i);
                dst.copy_from_slice(&[y, y, y, sample(1, i)]);
            }
            3 => dst.copy_from_slice(&[sample(0, i), sample(1, i), sample(2, i), 255]),
            _ => dst.copy_from_slice(&[sample(0, i), sample(1, i), sample(2, i), sample(3, i)]),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode_bytes, decode_named_bytes, FitBox};

    // 3×2 solid rgb(10,20,30), every RAW/RLE × 8/16-bit combination.
    const RGB8_RLE: &[u8] = include_bytes!("../tests/fixtures/solid_3x2.psd");
    const RGB8_RAW: &[u8] = include_bytes!("../tests/fixtures/solid_3x2_raw.psd");
    const RGB16_RLE: &[u8] = include_bytes!("../tests/fixtures/solid_3x2_16_rle.psd");
    const RGB16_RAW: &[u8] = include_bytes!("../tests/fixtures/solid_3x2_16_raw.psd");
    // 4×2 solid 8-bit grayscale (1-channel), RLE.
    const GRAY8_RLE: &[u8] = include_bytes!("../tests/fixtures/gray_4x2_rle.psd");

    fn req(bytes: &[u8]) -> DecodeRequest<'_> {
        DecodeRequest {
            bytes,
            fit: None,
            allow_preview: false,
        }
    }

    #[test]
    fn can_decode_sniffs_8bps_magic() {
        let d = PsdDecoder;
        assert!(d.can_decode(b"8BPS\x00\x01"));
        assert!(d.can_decode(RGB8_RLE));
        assert!(!d.can_decode(&[0xFF, 0xD8, 0xFF])); // JPEG
        assert!(!d.can_decode(b"8BP")); // too short / not the full signature
    }

    /// Every RGB compression/depth combination decodes to the identical exact
    /// composite — this is the matrix zune-psd got wrong (16-bit RLE was the file
    /// the user hit; uncompressed 8-bit was garbled).
    #[test]
    fn every_rgb_variant_decodes_to_expected_rgba() {
        for (label, bytes) in [
            ("rgb8-rle", RGB8_RLE),
            ("rgb8-raw", RGB8_RAW),
            ("rgb16-rle", RGB16_RLE),
            ("rgb16-raw", RGB16_RAW),
        ] {
            let img = PsdDecoder
                .decode(&req(bytes))
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            assert_eq!(img.codec, "PSD", "{label}");
            assert_eq!((img.orig_width, img.orig_height), (3, 2), "{label}");
            assert!(img.is_well_formed(), "{label}");
            for px in img.pixels.chunks_exact(4) {
                assert_eq!(px, &[10, 20, 30, 255], "{label}: pixel {px:?}");
            }
        }
    }

    #[test]
    fn grayscale_expands_to_opaque_gray() {
        let img = PsdDecoder.decode(&req(GRAY8_RLE)).expect("gray decode");
        assert_eq!((img.orig_width, img.orig_height), (4, 2));
        assert!(img.is_well_formed());
        for px in img.pixels.chunks_exact(4) {
            assert_eq!((px[0], px[1], px[2], px[3]), (px[0], px[0], px[0], 255));
            assert!((120..=136).contains(&px[0]), "gray ~128, got {}", px[0]);
        }
    }

    #[test]
    fn decode_to_fit_downscales_the_composite() {
        let img = PsdDecoder
            .decode(&DecodeRequest {
                bytes: RGB8_RLE,
                fit: Some(FitBox {
                    max_width: 1,
                    max_height: 1,
                }),
                allow_preview: false,
            })
            .expect("psd decode");
        assert_eq!((img.orig_width, img.orig_height), (3, 2));
        assert!(img.width <= 1 && img.height <= 1);
        assert!(img.is_well_formed());
    }

    #[test]
    fn content_sniff_routes_a_mislabeled_psd() {
        let by_content = decode_bytes(RGB16_RLE, None, false).expect("sniff decode");
        assert_eq!(by_content.codec, "PSD");
        let by_name = decode_named_bytes("mystery.dat", RGB16_RLE, None, false).expect("named");
        assert_eq!(by_name.codec, "PSD");
    }

    #[test]
    fn corrupt_8bps_is_an_error_not_a_panic() {
        let garbage = b"8BPS\x00\x01\x00\x00\x00\x00\x00\x00\xde\xad\xbe\xef";
        match decode_bytes(garbage, None, false) {
            Err(DecodeError::Corrupt(_)) | Err(DecodeError::Unsupported) => {}
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_color_mode_is_a_clean_error() {
        // Flip the golden's color-mode byte to CMYK (4) — must decline, not mis-color.
        let mut cmyk = RGB8_RLE.to_vec();
        cmyk[25] = 4;
        assert!(matches!(
            decode_composite(&cmyk),
            Err(DecodeError::Corrupt(_))
        ));
    }

    /// Build a minimal PSD prefix (26-byte header + color-mode + image-resources
    /// section) carrying a Version-Info (1057) resource with the given
    /// `hasRealMergedData` byte. Enough to exercise `composite_is_placeholder`.
    fn psd_with_version_info(has_real: u8) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"8BPS"); // signature
        v.extend_from_slice(&1u16.to_be_bytes()); // version 1
        v.extend_from_slice(&[0u8; 6]); // reserved
        v.extend_from_slice(&3u16.to_be_bytes()); // channels
        v.extend_from_slice(&2u32.to_be_bytes()); // height
        v.extend_from_slice(&3u32.to_be_bytes()); // width
        v.extend_from_slice(&8u16.to_be_bytes()); // depth
        v.extend_from_slice(&3u16.to_be_bytes()); // mode RGB
        v.extend_from_slice(&0u32.to_be_bytes()); // color-mode-data length 0
                                                  // One 8BIM block: id 1057, empty name, data = version(4) + hasRealMergedData(1) + pad.
        let mut block = Vec::new();
        block.extend_from_slice(b"8BIM");
        block.extend_from_slice(&1057u16.to_be_bytes());
        block.extend_from_slice(&[0u8, 0u8]); // empty Pascal name, padded to even
        let data = [1u8, 0, 0, 0, has_real]; // version=…, hasRealMergedData
        block.extend_from_slice(&(data.len() as u32).to_be_bytes());
        block.extend_from_slice(&data);
        block.push(0); // pad data to even (len 5 → 6)
        v.extend_from_slice(&(block.len() as u32).to_be_bytes()); // image-resources length
        v.extend_from_slice(&block);
        v
    }

    #[test]
    fn placeholder_composite_is_detected_and_skipped() {
        // hasRealMergedData == 0 → placeholder → detected + a clean decode error.
        assert!(composite_is_placeholder(&psd_with_version_info(0)));
        assert!(!composite_is_placeholder(&psd_with_version_info(1)));
        // Real fixtures (ImageMagick omits resource 1057) are never flagged.
        assert!(!composite_is_placeholder(RGB8_RLE));
        assert!(!composite_is_placeholder(RGB16_RLE));
    }

    #[test]
    fn image_data_offset_is_bounds_safe_on_truncation() {
        let mut b = RGB8_RAW[..HEADER_LEN].to_vec();
        b.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xF0]); // absurd color-mode length
        assert_eq!(image_data_offset(&b), None);
    }

    #[test]
    fn unpack_bits_handles_literal_and_run_and_noop() {
        let mut out = Vec::new();
        // 0x02 → 3 literal bytes [1,2,3]; 0xFE (−2) → repeat next byte ×3; 0x80 → noop.
        let src = [0x02, 1, 2, 3, 0xFE, 9, 0x80];
        unpack_bits_into(&src, 6, &mut out).expect("unpack");
        assert_eq!(out, vec![1, 2, 3, 9, 9, 9]);
        // A run that would overrun the expected length is rejected.
        let mut o2 = Vec::new();
        assert!(unpack_bits_into(&[0x02, 1, 2, 3], 2, &mut o2).is_none());
    }
}
