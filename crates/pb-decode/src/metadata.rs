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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_exif_is_empty() {
        // A minimal JPEG (SOI + EOI) carries no EXIF.
        assert!(read_exif_fields(&[0xFF, 0xD8, 0xFF, 0xD9]).is_empty());
        assert!(read_exif_fields(&[]).is_empty());
    }
}
