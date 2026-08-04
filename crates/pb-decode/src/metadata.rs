//! Full EXIF metadata extraction for the "nerd mode" panel (tasks.json #5).
//!
//! Reads every EXIF field as `(tag, value)` display strings from in-memory bytes.
//! Privacy (task #2): on-demand from RAM only — nothing is cached to disk.

use std::io::Cursor;

/// All EXIF fields as `(tag, display value)` pairs, in file order. Empty when the
/// bytes carry no EXIF (or aren't a container `exif` understands).
pub fn read_exif_fields(bytes: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut cursor = Cursor::new(bytes);
    if let Ok(exif) = exif::Reader::new().read_from_container(&mut cursor) {
        for f in exif.fields() {
            let tag = f.tag.to_string();
            out.push((tag, field_value(f, &exif)));
        }
    }
    out
}

/// A field's display string. ASCII fields are decoded as UTF-8 (lossy) so
/// multibyte text shows as real characters — e.g. a copyright "©" stored as the
/// UTF-8 bytes `0xC2 0xA9` renders as `©`, not `display_value`'s `\xc2\xa9`
/// escapes. Every other type keeps the crate's formatting, which carries units
/// (e.g. `f/2.8`, `1/250 s`, `50 mm`).
fn field_value(f: &exif::Field, exif: &exif::Exif) -> String {
    match &f.value {
        exif::Value::Ascii(parts) => parts
            .iter()
            .map(|b| {
                String::from_utf8_lossy(b)
                    .trim_end_matches('\0')
                    .trim()
                    .to_string()
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        _ => f.display_value().with_unit(exif).to_string(),
    }
}

/// The EXIF `UserComment` as text, decoded from its **raw bytes** (task #137).
///
/// This exists because [`read_exif_fields`] cannot serve it. `UserComment` is an
/// `Undefined` value, and [`field_value`] only byte-decodes `Value::Ascii` —
/// everything else goes through `display_value()`, which renders the payload as
/// escapes. By the time a caller sees the flat `(tag, value)` list the text is
/// already mangled, so the decode has to happen here, while the raw field bytes
/// still exist.
///
/// The Automatic1111 family writes its whole `parameters` block here for JPEG
/// and WebP output, which is the only reason we care.
///
/// Per EXIF 2.3 the value opens with an 8-byte character-code designator:
/// `ASCII\0\0\0`, `UNICODE\0`, `JIS\0\0\0\0\0`, or eight NULs for "undefined".
/// `UNICODE` means UTF-16 in EXIF's world; endianness is taken from a BOM when
/// present and otherwise follows the TIFF byte order, for which little-endian is
/// the overwhelmingly common case. `JIS` is not decoded (no sample, and guessing
/// an encoding is exactly what this function exists to avoid).
pub fn read_exif_user_comment(bytes: &[u8]) -> Option<String> {
    let mut cursor = Cursor::new(bytes);
    let exif = exif::Reader::new().read_from_container(&mut cursor).ok()?;
    let field = exif.get_field(exif::Tag::UserComment, exif::In::PRIMARY)?;
    let exif::Value::Undefined(raw, _) = &field.value else {
        return None;
    };
    let (designator, body) = raw.split_at_checked(8)?;
    let text = match designator {
        b"ASCII\0\0\0" => String::from_utf8_lossy(body).into_owned(),
        b"UNICODE\0" => decode_utf16(body)?,
        // Eight NULs = "undefined" — the writer declined to say. Real files in
        // this state hold plain UTF-8, so read it as such rather than discard a
        // payload we can plainly see.
        [0, 0, 0, 0, 0, 0, 0, 0] => String::from_utf8_lossy(body).into_owned(),
        _ => return None,
    };
    let text = text.trim_end_matches('\0').trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// UTF-16 bytes to a `String`, honoring a leading BOM and defaulting to
/// little-endian without one. Odd-length input is rejected rather than padded.
fn decode_utf16(body: &[u8]) -> Option<String> {
    let (body, big_endian) = match body {
        [0xFE, 0xFF, rest @ ..] => (rest, true),
        [0xFF, 0xFE, rest @ ..] => (rest, false),
        _ => (body, false),
    };
    if body.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = body
        .chunks_exact(2)
        .map(|c| {
            if big_endian {
                u16::from_be_bytes([c[0], c[1]])
            } else {
                u16::from_le_bytes([c[0], c[1]])
            }
        })
        .collect();
    Some(String::from_utf16_lossy(&units))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_decodes_by_bom_and_defaults_little_endian() {
        // The BOM wins when present, in both directions...
        let be: Vec<u8> = [0xFE, 0xFF]
            .into_iter()
            .chain([0x00, b'h', 0x00, b'i'])
            .collect();
        assert_eq!(decode_utf16(&be).as_deref(), Some("hi"));
        let le: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain([b'h', 0x00, b'i', 0x00])
            .collect();
        assert_eq!(decode_utf16(&le).as_deref(), Some("hi"));
        // ...and bare payloads fall back to little-endian, the common case.
        assert_eq!(
            decode_utf16(&[b'h', 0x00, b'i', 0x00]).as_deref(),
            Some("hi")
        );
        // Odd length is rejected rather than padded into mojibake.
        assert_eq!(decode_utf16(&[b'h', 0x00, b'i']), None);
    }

    #[test]
    fn user_comment_needs_real_exif() {
        // The unit-level guard; the container-level path is exercised by the
        // pb-app-core integration tests, which own real fixture bytes.
        assert_eq!(read_exif_user_comment(&[]), None);
        assert_eq!(read_exif_user_comment(&[0xFF, 0xD8, 0xFF, 0xD9]), None);
    }

    #[test]
    fn no_exif_is_empty() {
        // A minimal JPEG (SOI + EOI) carries no EXIF.
        assert!(read_exif_fields(&[0xFF, 0xD8, 0xFF, 0xD9]).is_empty());
        assert!(read_exif_fields(&[]).is_empty());
    }
}
