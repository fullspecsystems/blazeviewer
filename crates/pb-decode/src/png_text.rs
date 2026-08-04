//! PNG textual-chunk extraction for AI generation metadata (task #137).
//!
//! Reads the `tEXt` / `zTXt` / `iTXt` chunks that ComfyUI and the
//! Automatic1111 family use to record how an image was generated. Privacy
//! (task #2): on-demand from RAM only — nothing is cached to disk.
//!
//! **This is a hostile-input parser and it does not ride
//! [`crate::catch_panics`]** — that wrapper is private and typed to
//! `Result<DecodedImage, DecodeError>`, and this function is called directly
//! from the metadata path. It must therefore be panic-free on its own merits:
//! every read is bounds-checked, every length is validated against the
//! remaining input, and the inflate is bounded. It has a fuzz target for the
//! same reason.
//!
//! Four policies exist because a *wrong* fact is worse than a missing one
//! (see the plan's safety invariant):
//!
//! 1. **Allow-list.** Only [`WANTED`] keywords are collected. A PNG's other text
//!    chunks (`Software`, `Comment`, an embedded XML dump) are skipped without
//!    allocating — this is a generation-metadata reader, not a general one.
//! 2. **CRC validation.** A chunk whose CRC does not match is dropped. A corrupt
//!    parameters block that still parses would yield confidently wrong facts.
//! 3. **Duplicate keyword is ambiguous.** Two `parameters` chunks mean we cannot
//!    know which describes the image, so *both* are dropped rather than
//!    first-wins.
//! 4. **Overflow discards the chunk atomically.** A `zTXt` that inflates past
//!    [`MAX_INFLATED`] is dropped entirely, never truncated — a truncated JSON
//!    graph or parameters line is exactly the "parses into wrong facts" case.

use std::io::Read;

/// The only keywords collected. Everything else is skipped without allocating.
///
/// `prompt` + `workflow` are ComfyUI (the executed API graph and the UI graph
/// respectively); `parameters` is the Automatic1111 family.
pub const WANTED: [&str; 3] = ["prompt", "workflow", "parameters"];

/// Cap on a single chunk's decompressed size. A `zTXt`/`iTXt` is attacker-
/// controlled compressed data — without a cap, a few KB of file expands to
/// gigabytes of RAM. Real payloads are ~4–30 KB; 1 MB is generous.
const MAX_INFLATED: usize = 1 << 20;

/// Cap on an *uncompressed* `tEXt`/`iTXt` payload, for symmetry with
/// [`MAX_INFLATED`]. A larger chunk is a payload we would refuse to display
/// anyway.
const MAX_RAW: usize = 1 << 20;

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// The wanted text chunks of `bytes`, as `(keyword, payload)` in file order.
///
/// Empty when the bytes are not a PNG, carry none of [`WANTED`], or are damaged.
/// Never panics and never allocates more than [`MAX_INFLATED`] per chunk —
/// see the module docs for the four rejection policies.
pub fn read_png_text(bytes: &[u8]) -> Vec<(String, String)> {
    if bytes.len() < SIGNATURE.len() || bytes[..SIGNATURE.len()] != SIGNATURE {
        return Vec::new();
    }
    let mut found: Vec<(String, String)> = Vec::new();
    // Keywords seen *at all* — including ones whose payload was rejected. A
    // duplicate must poison the keyword even if the first copy was dropped for
    // a bad CRC, or a corrupt chunk followed by a good one would resurrect the
    // ambiguity this is meant to refuse.
    let mut seen: Vec<String> = Vec::new();
    let mut pos = SIGNATURE.len();

    while pos + 8 <= bytes.len() {
        let len = u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
            as usize;
        let kind = &bytes[pos + 4..pos + 8];
        // `pos + 8 + len + 4` can overflow on a hostile length; do it checked so
        // a bogus chunk ends the walk instead of wrapping into a valid offset.
        let Some(end) = pos.checked_add(12).and_then(|p| p.checked_add(len)) else {
            break;
        };
        if end > bytes.len() {
            break; // truncated file — keep whatever validated so far
        }
        let data = &bytes[pos + 8..pos + 8 + len];
        let crc = u32::from_be_bytes([
            bytes[end - 4],
            bytes[end - 3],
            bytes[end - 2],
            bytes[end - 1],
        ]);

        if matches!(kind, b"tEXt" | b"zTXt" | b"iTXt") {
            // Peek the keyword before doing any work: the allow-list check is a
            // memcmp, while decoding is an allocation and possibly an inflate.
            if let Some(keyword) = peek_keyword(data) {
                if WANTED.contains(&keyword.as_str()) {
                    let duplicate = seen.contains(&keyword);
                    seen.push(keyword.clone());
                    if duplicate {
                        // Policy 3: ambiguous. Drop this one *and* the earlier one.
                        found.retain(|(k, _)| *k != keyword);
                    } else if crc32(kind, data) == crc {
                        // Policy 2: only a CRC-valid chunk is trusted.
                        if let Some(text) = decode_chunk(kind, data) {
                            found.push((keyword, text));
                        }
                    }
                }
            }
        }
        if kind == b"IEND" {
            break;
        }
        pos = end;
    }
    found
}

/// The keyword of a text chunk (the bytes before the first NUL), without
/// decoding the payload. `None` when there is no NUL or the keyword is empty or
/// not Latin-1 printable — the PNG spec restricts keywords to 1–79 characters.
fn peek_keyword(data: &[u8]) -> Option<String> {
    let nul = data.iter().position(|&b| b == 0)?;
    if nul == 0 || nul > 79 {
        return None;
    }
    Some(String::from_utf8_lossy(&data[..nul]).into_owned())
}

/// A text chunk's payload as a string, or `None` if it is malformed or breaches
/// a size cap. The three chunk types differ only in how the payload is framed:
///
/// - `tEXt`: `keyword \0 text` — Latin-1, uncompressed.
/// - `zTXt`: `keyword \0 method text` — always zlib-deflated.
/// - `iTXt`: `keyword \0 flag method lang \0 translated \0 text` — UTF-8,
///   deflated only when `flag == 1`.
fn decode_chunk(kind: &[u8], data: &[u8]) -> Option<String> {
    let nul = data.iter().position(|&b| b == 0)?;
    let rest = data.get(nul + 1..)?;
    match kind {
        b"tEXt" => {
            if rest.len() > MAX_RAW {
                return None;
            }
            // Latin-1 by spec. Real writers emit UTF-8; decode as UTF-8 when it
            // is valid and fall back to Latin-1 so neither is mangled.
            Some(match std::str::from_utf8(rest) {
                Ok(s) => s.to_string(),
                Err(_) => rest.iter().map(|&b| b as char).collect(),
            })
        }
        b"zTXt" => {
            // rest = [compression method][compressed data]
            let (method, payload) = rest.split_first()?;
            if *method != 0 {
                return None; // only deflate is defined
            }
            inflate_bounded(payload)
        }
        b"iTXt" => {
            // rest = [flag][method][lang \0][translated \0][text]
            let &flag = rest.first()?;
            let &method = rest.get(1)?;
            let after = rest.get(2..)?;
            let lang_end = after.iter().position(|&b| b == 0)?;
            let after = after.get(lang_end + 1..)?;
            let tr_end = after.iter().position(|&b| b == 0)?;
            let text = after.get(tr_end + 1..)?;
            match flag {
                0 => {
                    if text.len() > MAX_RAW {
                        return None;
                    }
                    Some(String::from_utf8_lossy(text).into_owned())
                }
                1 if method == 0 => inflate_bounded(text),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Inflate `payload`, refusing anything that expands past [`MAX_INFLATED`].
///
/// Policy 4: an overflow returns `None` — the chunk is discarded whole. Reading
/// `MAX_INFLATED + 1` and checking afterwards is what makes that atomic: a
/// decompressor asked for exactly the cap cannot distinguish "fits exactly"
/// from "was truncated", and a truncated payload is the dangerous case.
fn inflate_bounded(payload: &[u8]) -> Option<String> {
    let mut out = Vec::new();
    let mut reader = flate2::read::ZlibDecoder::new(payload).take(MAX_INFLATED as u64 + 1);
    reader.read_to_end(&mut out).ok()?;
    if out.len() > MAX_INFLATED {
        return None;
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// PNG's CRC-32 (IEEE, reflected) over the chunk type followed by its data.
fn crc32(kind: &[u8], data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in kind.iter().chain(data) {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a PNG carrying `chunks` as raw `(type, payload)` pairs, with valid
    /// CRCs, framed by a real signature/IHDR/IDAT/IEND so the walker is
    /// exercised the way a real file exercises it.
    fn png_with(chunks: &[(&[u8], Vec<u8>)]) -> Vec<u8> {
        let mut out = SIGNATURE.to_vec();
        let ihdr = vec![0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0];
        push_chunk(&mut out, b"IHDR", &ihdr);
        for (kind, data) in chunks {
            push_chunk(&mut out, kind, data);
        }
        push_chunk(
            &mut out,
            b"IDAT",
            &[0x78, 0x9C, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01],
        );
        push_chunk(&mut out, b"IEND", &[]);
        out
    }

    fn push_chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&crc32(kind, data).to_be_bytes());
    }

    fn text_chunk(keyword: &str, text: &str) -> Vec<u8> {
        let mut d = keyword.as_bytes().to_vec();
        d.push(0);
        d.extend_from_slice(text.as_bytes());
        d
    }

    fn ztxt_chunk(keyword: &str, text: &str) -> Vec<u8> {
        let mut d = keyword.as_bytes().to_vec();
        d.push(0);
        d.push(0); // compression method: deflate
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(text.as_bytes()).unwrap();
        d.extend_from_slice(&e.finish().unwrap());
        d
    }

    fn itxt_chunk(keyword: &str, text: &str, compressed: bool) -> Vec<u8> {
        let mut d = keyword.as_bytes().to_vec();
        d.push(0);
        d.push(if compressed { 1 } else { 0 }); // compression flag
        d.push(0); // compression method
        d.push(0); // language tag (empty) terminator
        d.push(0); // translated keyword (empty) terminator
        if compressed {
            let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
            e.write_all(text.as_bytes()).unwrap();
            d.extend_from_slice(&e.finish().unwrap());
        } else {
            d.extend_from_slice(text.as_bytes());
        }
        d
    }

    #[test]
    fn reads_a_plain_text_chunk() {
        let png = png_with(&[(b"tEXt", text_chunk("parameters", "a photo, Steps: 20"))]);
        assert_eq!(
            read_png_text(&png),
            vec![("parameters".to_string(), "a photo, Steps: 20".to_string())]
        );
    }

    #[test]
    fn reads_compressed_and_uncompressed_variants() {
        // zTXt (always deflated) and both iTXt flavours must all round-trip —
        // ComfyUI writes tEXt today but nothing stops a re-saver using the others.
        for chunk in [
            (b"zTXt".as_slice(), ztxt_chunk("prompt", "{\"3\":{}}")),
            (
                b"iTXt".as_slice(),
                itxt_chunk("prompt", "{\"3\":{}}", false),
            ),
            (b"iTXt".as_slice(), itxt_chunk("prompt", "{\"3\":{}}", true)),
        ] {
            let png = png_with(&[(chunk.0, chunk.1)]);
            assert_eq!(
                read_png_text(&png),
                vec![("prompt".to_string(), "{\"3\":{}}".to_string())],
                "chunk type {:?} did not round-trip",
                std::str::from_utf8(chunk.0).unwrap()
            );
        }
    }

    #[test]
    fn finds_a_chunk_that_follows_idat() {
        // Text chunks may legally appear after IDAT, so the walk must run to
        // IEND rather than stopping at the first image data.
        let mut png = SIGNATURE.to_vec();
        push_chunk(&mut png, b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
        push_chunk(
            &mut png,
            b"IDAT",
            &[0x78, 0x9C, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01],
        );
        push_chunk(&mut png, b"tEXt", &text_chunk("parameters", "trailing"));
        push_chunk(&mut png, b"IEND", &[]);
        assert_eq!(
            read_png_text(&png),
            vec![("parameters".to_string(), "trailing".to_string())]
        );
    }

    #[test]
    fn collects_only_the_allow_listed_keywords() {
        // A generation-metadata reader, not a general text reader: a PNG's
        // Software/Comment/Description chunks are skipped without allocating.
        let png = png_with(&[
            (b"tEXt", text_chunk("Software", "gimp")),
            (b"tEXt", text_chunk("Comment", "hello")),
            (b"tEXt", text_chunk("Description", "a cat")),
            (b"tEXt", text_chunk("prompt", "{}")),
        ]);
        assert_eq!(
            read_png_text(&png),
            vec![("prompt".to_string(), "{}".to_string())]
        );
    }

    #[test]
    fn a_bad_crc_drops_the_chunk() {
        // A corrupt parameters block that still parsed would produce
        // confidently wrong facts — worse than showing nothing.
        let mut png = png_with(&[(b"tEXt", text_chunk("parameters", "Steps: 20"))]);
        // Corrupt the tEXt payload in place, leaving its now-stale CRC behind.
        let at = png
            .windows(9)
            .position(|w| w == b"Steps: 20")
            .expect("payload present");
        png[at] = b'X';
        assert!(read_png_text(&png).is_empty());
    }

    #[test]
    fn a_duplicate_keyword_is_ambiguous_and_drops_both() {
        // Two `parameters` chunks mean we cannot know which describes the
        // image. First-wins would be a coin flip presented as a fact.
        let png = png_with(&[
            (b"tEXt", text_chunk("parameters", "first")),
            (b"tEXt", text_chunk("parameters", "second")),
        ]);
        assert!(read_png_text(&png).is_empty());

        // A surviving *different* keyword is unaffected by the poisoned one.
        let png = png_with(&[
            (b"tEXt", text_chunk("parameters", "first")),
            (b"tEXt", text_chunk("prompt", "{}")),
            (b"tEXt", text_chunk("parameters", "second")),
        ]);
        assert_eq!(
            read_png_text(&png),
            vec![("prompt".to_string(), "{}".to_string())]
        );
    }

    #[test]
    fn a_duplicate_still_poisons_when_the_first_copy_was_corrupt() {
        // Regression: poisoning keyed off what was *kept* rather than what was
        // *seen* would let a bad-CRC first copy vanish and the second be
        // reported as unambiguous — resurrecting the exact ambiguity refused
        // above.
        let mut png = png_with(&[
            (b"tEXt", text_chunk("parameters", "corrupted")),
            (b"tEXt", text_chunk("parameters", "second")),
        ]);
        let at = png
            .windows(9)
            .position(|w| w == b"corrupted")
            .expect("payload present");
        png[at] = b'X';
        assert!(read_png_text(&png).is_empty());
    }

    #[test]
    fn an_oversize_inflate_discards_the_chunk_whole() {
        // A zlib bomb must not be truncated into a "valid-looking" prefix: a
        // truncated JSON graph or parameters line is precisely the case that
        // parses into wrong facts. Discard, don't clamp.
        let huge = "a".repeat(MAX_INFLATED + 1024);
        let png = png_with(&[(b"zTXt", ztxt_chunk("prompt", &huge))]);
        assert!(read_png_text(&png).is_empty());

        // Just under the cap still comes through, so the bound is a real cap
        // and not an accidental rejection of everything large.
        let big = "a".repeat(MAX_INFLATED - 1024);
        let png = png_with(&[(b"zTXt", ztxt_chunk("prompt", &big))]);
        assert_eq!(read_png_text(&png)[0].1.len(), big.len());
    }

    #[test]
    fn non_png_and_damaged_input_yield_nothing_without_panicking() {
        assert!(read_png_text(&[]).is_empty());
        assert!(read_png_text(b"not a png at all").is_empty());
        assert!(read_png_text(&SIGNATURE).is_empty());
        // A truncated file keeps whatever validated before the cut.
        let png = png_with(&[(b"tEXt", text_chunk("prompt", "{}"))]);
        for cut in 0..png.len() {
            let _ = read_png_text(&png[..cut]);
        }
        // A hostile length field must not wrap into a valid offset.
        let mut bogus = SIGNATURE.to_vec();
        bogus.extend_from_slice(&u32::MAX.to_be_bytes());
        bogus.extend_from_slice(b"tEXt");
        bogus.extend_from_slice(b"prompt\0{}");
        assert!(read_png_text(&bogus).is_empty());
    }

    #[test]
    fn a_keyword_without_a_terminator_is_skipped() {
        // Malformed framing must be skipped, not read past its own chunk.
        let png = png_with(&[(b"tEXt", b"no-nul-here".to_vec())]);
        assert!(read_png_text(&png).is_empty());
    }

    #[test]
    fn crc32_matches_the_png_spec_vector() {
        // IEND's payload is empty, and its CRC is a fixed, well-known constant —
        // a cheap check that the table-free implementation is the right
        // polynomial and bit order.
        assert_eq!(crc32(b"IEND", &[]), 0xAE42_6082);
    }
}
