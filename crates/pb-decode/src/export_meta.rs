//! The metadata an export may copy (export-plan.md § 3.7), read from the source file's bytes
//! beside the pixel decode: its EXIF block, located per container, and a PNG's AI generation
//! text (task #137's reader). Raw, as found: the allowlist lives in
//! `pb_encode::SourceMetadata::output`, which is the only thing that decides what is written.
//!
//! Where the EXIF block comes from:
//!
//! - JPEG `APP1`, PNG `eXIf`, WebP `EXIF`, HEIF/HEIC/AVIF `Exif` item: kamadak-exif's
//!   container reader, the one the viewer reads orientation with.
//! - TIFF and TIFF-structured camera RAW (DNG, NEF, CR2, ARW, … plus Olympus ORF and Panasonic
//!   RW2, whose magic numbers differ): the file *is* the EXIF structure, so instead of holding
//!   a copy of the whole file it is compacted straight to its allowlisted superset
//!   (`pb_encode::metadata::compact_exif`).
//! - JPEG XL: the container's uncompressed `Exif` box. A Brotli-compressed (`brob`) one is
//!   skipped, as are formats with no EXIF reader here (Fuji RAF, Canon CR3).
//!
//! Read-only and RAM-only (privacy #2): nothing is cached or written. Cheap enough for the
//! export worker to run per file; panic-safe like the decode.

use std::borrow::Cow;
use std::io::Cursor;
use std::sync::Arc;

use pb_encode::SourceMetadata;

/// What `bytes` carries that an export may copy: its EXIF block and, for a PNG, its generation
/// text. Empty for anything without either (or unreadable). Never panics.
pub fn read_export_metadata(bytes: &[u8]) -> SourceMetadata {
    std::panic::catch_unwind(|| SourceMetadata {
        exif: exif_block(bytes).map(Arc::from),
        generation: crate::png_text::read_png_text(bytes),
    })
    .unwrap_or_default()
}

/// The source's EXIF payload (a TIFF structure), wherever its container keeps it.
fn exif_block(bytes: &[u8]) -> Option<Vec<u8>> {
    if let Some(tiff) = tiff_structured(bytes) {
        return pb_encode::metadata::compact_exif(&tiff);
    }
    if let Some(block) = jxl_exif(bytes) {
        return Some(block.to_vec());
    }
    let exif = exif::Reader::new()
        .read_from_container(&mut Cursor::new(bytes))
        .ok()?;
    Some(exif.buf().to_vec())
}

/// The file as a TIFF structure, when it is one: borrowed for a plain TIFF, or a copy with the
/// standard magic number for the RAW formats that use their own.
fn tiff_structured(bytes: &[u8]) -> Option<Cow<'_, [u8]>> {
    let patched = |magic: &[u8; 4]| {
        let mut v = bytes.to_vec();
        v[..4].copy_from_slice(magic);
        Cow::Owned(v)
    };
    match bytes.get(..4)? {
        b"II*\0" | b"MM\0*" => Some(Cow::Borrowed(bytes)),
        // Olympus ORF ("IIRO", "IIRS") and Panasonic RW2 ("IIU\0").
        b"IIRO" | b"IIRS" | b"IIU\0" => Some(patched(b"II*\0")),
        b"MMOR" => Some(patched(b"MM\0*")),
        _ => None,
    }
}

/// A JPEG XL container's `Exif` box payload, past its 4-byte TIFF-header offset.
fn jxl_exif(bytes: &[u8]) -> Option<&[u8]> {
    const SIGNATURE: &[u8] = b"\0\0\0\x0CJXL \r\n\x87\n";
    if !bytes.starts_with(SIGNATURE) {
        return None;
    }
    let be32 = |at: usize| -> Option<usize> {
        Some(u32::from_be_bytes(bytes.get(at..at.checked_add(4)?)?.try_into().ok()?) as usize)
    };
    let mut at = 0usize;
    while at.checked_add(8)? <= bytes.len() {
        let size32 = be32(at)?;
        let typ = bytes.get(at + 4..at + 8)?;
        let (hdr, size) = match size32 {
            0 => (8, bytes.len() - at),
            1 => {
                let wide = u64::from_be_bytes(bytes.get(at + 8..at + 16)?.try_into().ok()?);
                (16, usize::try_from(wide).ok()?)
            }
            n => (8, n),
        };
        if size < hdr {
            return None;
        }
        let end = at.checked_add(size)?;
        let body = bytes.get(at + hdr..end)?;
        if typ == b"Exif" {
            let offset = u32::from_be_bytes(body.get(..4)?.try_into().ok()?) as usize;
            return body.get(4usize.checked_add(offset)?..);
        }
        at = end;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use exif::experimental::Writer;
    use exif::{Field, In, Rational, Tag, Value};
    use pb_encode::{MetadataChoice, OutputMetadata, PrepareSpec};

    fn field(tag: Tag, value: Value) -> Field {
        Field {
            tag,
            ifd_num: In::PRIMARY,
            value,
        }
    }

    fn ascii(s: &str) -> Value {
        Value::Ascii(vec![s.as_bytes().to_vec()])
    }

    fn rat(n: u32, d: u32) -> Rational {
        Rational { num: n, denom: d }
    }

    /// A camera's EXIF: what an export may keep beside what it must never leak.
    fn dirty_exif() -> Vec<u8> {
        let fields = [
            field(Tag::Make, ascii("NIKON CORPORATION")),
            field(Tag::Model, ascii("NIKON Z 8")),
            field(Tag::Artist, ascii("A. Photographer")),
            field(Tag::Orientation, Value::Short(vec![6])),
            field(Tag::DateTimeOriginal, ascii("2025:01:02 03:04:05")),
            field(Tag::FNumber, Value::Rational(vec![rat(4, 1)])),
            field(
                Tag::MakerNote,
                Value::Undefined(b"NIKON-PRIVATE".to_vec(), 0),
            ),
            field(Tag::BodySerialNumber, ascii("SERIAL-0042")),
            field(
                Tag::UserComment,
                Value::Undefined(b"ASCII\0\0\0private comment".to_vec(), 0),
            ),
            field(Tag::GPSLatitudeRef, ascii("S")),
            field(
                Tag::GPSLatitude,
                Value::Rational(vec![rat(33, 1), rat(51, 1), rat(54, 1)]),
            ),
            field(Tag::GPSLongitudeRef, ascii("E")),
            field(
                Tag::GPSLongitude,
                Value::Rational(vec![rat(151, 1), rat(12, 1), rat(36, 1)]),
            ),
        ];
        let mut w = Writer::new();
        fields.iter().for_each(|f| w.push_field(f));
        let mut out = Cursor::new(Vec::new());
        w.write(&mut out, false).unwrap();
        out.into_inner()
    }

    /// A JPEG of a 4×2 landscape whose EXIF says "rotate 90°" (Orientation 6) and carries
    /// GPS, a MakerNote, a serial number and a UserComment.
    fn dirty_jpeg() -> Vec<u8> {
        let img = pb_encode::ExportImage {
            width: 4,
            height: 2,
            channels: pb_encode::Channels::Rgb,
            samples: pb_encode::Samples::U8(vec![128; 4 * 2 * 3]),
            color: pb_encode::OutputColor::Srgb,
            notes: Vec::new(),
            metadata: OutputMetadata {
                exif: Some(dirty_exif()),
                text: Vec::new(),
            },
        };
        pb_encode::encode_jpeg(&img, 90).unwrap()
    }

    /// The primary-image tags of a file, by an independent reader.
    fn tags_of(file: &[u8]) -> Option<Vec<(Tag, String)>> {
        let exif = exif::Reader::new()
            .read_from_container(&mut Cursor::new(file))
            .ok()?;
        Some(
            exif.fields()
                .filter(|f| f.ifd_num == In::PRIMARY)
                .map(|f| (f.tag, f.display_value().to_string()))
                .collect(),
        )
    }

    fn has(tags: &[(Tag, String)], tag: Tag) -> bool {
        tags.iter().any(|(t, _)| *t == tag)
    }

    /// Required test 1, end to end through the library: decode the JPEG for export, read its
    /// metadata, prepare and encode it. Camera + copyright: make, model, date and artist
    /// survive, Orientation is 1 (the pixels were turned upright), and no GPS, MakerNote,
    /// serial number or UserComment. Location adds the GPS. Strip writes no EXIF at all.
    #[test]
    fn a_jpeg_export_copies_only_the_chosen_categories() {
        let jpeg = dirty_jpeg();
        let export = |spec: PrepareSpec| {
            let src = crate::decode_named_bytes_for_export("dirty.jpg", &jpeg).unwrap();
            assert_eq!(
                (src.width, src.height),
                (2, 4),
                "Orientation 6 applied to the pixels"
            );
            let meta = read_export_metadata(&jpeg);
            let img = pb_encode::prepare_with_metadata(src, &meta, &spec).unwrap();
            pb_encode::encode_jpeg(&img, 90).unwrap()
        };
        let keep = PrepareSpec {
            metadata: MetadataChoice::REVEALED,
            ..PrepareSpec::full(0, false)
        };

        let tags = tags_of(&export(keep)).expect("EXIF written");
        for kept in [
            Tag::Make,
            Tag::Model,
            Tag::DateTimeOriginal,
            Tag::Artist,
            Tag::FNumber,
        ] {
            assert!(has(&tags, kept), "{kept} missing: {tags:?}");
        }
        let orientation = tags.iter().find(|(t, _)| *t == Tag::Orientation).unwrap();
        assert_eq!(orientation.1, "row 0 at top and column 0 at left");
        for gone in [
            Tag::GPSLatitude,
            Tag::GPSLongitude,
            Tag::MakerNote,
            Tag::BodySerialNumber,
            Tag::UserComment,
        ] {
            assert!(!has(&tags, gone), "{gone} leaked");
        }

        let with_location = PrepareSpec {
            metadata: MetadataChoice {
                location: true,
                ..MetadataChoice::REVEALED
            },
            ..keep
        };
        let tags = tags_of(&export(with_location)).unwrap();
        assert!(has(&tags, Tag::GPSLatitude) && has(&tags, Tag::GPSLongitudeRef));
        assert!(!has(&tags, Tag::MakerNote) && !has(&tags, Tag::BodySerialNumber));

        let strip = PrepareSpec {
            strip_metadata: true,
            ..with_location
        };
        let stripped = export(strip);
        assert!(tags_of(&stripped).is_none(), "Strip: no EXIF at all");
        assert!(!stripped.windows(6).any(|w| w == b"Exif\0\0"));
    }

    /// Required test 4 from a real file: what this JPEG has to offer.
    #[test]
    fn availability_from_a_jpeg() {
        let jpeg = dirty_jpeg();
        let src = crate::decode_named_bytes_for_export("dirty.jpg", &jpeg).unwrap();
        let meta = read_export_metadata(&jpeg);
        assert_eq!(
            meta.available(&src.color),
            MetadataChoice {
                camera: true,
                copyright: true,
                location: true,
                generation: false,
                original_profile: false,
            }
        );
        // A JPEG with no EXIF.
        let plain = pb_encode::encode_jpeg_rgba8(&[9; 4 * 4], 2, 2, 90).unwrap();
        assert_eq!(read_export_metadata(&plain), SourceMetadata::default());
    }

    /// A PNG with A1111 `parameters` (`tEXt`) and a ComfyUI `workflow` (`iTXt`, non-ASCII).
    fn generated_png() -> Vec<u8> {
        let mut info = png::Info::with_size(2, 1);
        info.color_type = png::ColorType::Rgb;
        info.bit_depth = png::BitDepth::Eight;
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::with_info(&mut out, info).unwrap();
            enc.add_text_chunk("parameters".into(), "a lighthouse, Steps: 30".into())
                .unwrap();
            enc.add_itxt_chunk("workflow".into(), "{\"nodes\": [\"Überblick\"]}".into())
                .unwrap();
            enc.add_text_chunk("Software".into(), "not generation data".into())
                .unwrap();
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[1, 2, 3, 4, 5, 6]).unwrap();
        }
        out
    }

    /// Required test 3: PNG → PNG carries the generation text when chosen, and only then.
    #[test]
    fn generation_text_round_trips_png_to_png() {
        let png = generated_png();
        let meta = read_export_metadata(&png);
        assert_eq!(
            meta.generation,
            vec![
                (
                    "parameters".to_string(),
                    "a lighthouse, Steps: 30".to_string()
                ),
                (
                    "workflow".to_string(),
                    "{\"nodes\": [\"Überblick\"]}".to_string()
                ),
            ]
        );
        assert!(meta.available(&pb_encode::ColorDesc::Untagged).generation);
        let export = |metadata: MetadataChoice| {
            let src = crate::decode_named_bytes_for_export("gen.png", &png).unwrap();
            let spec = PrepareSpec {
                metadata,
                ..PrepareSpec::full(0, false)
            };
            let img = pb_encode::prepare_with_metadata(src, &meta, &spec).unwrap();
            pb_encode::encode_png(&img, pb_encode::PngEffort::Fast).unwrap()
        };
        let out = export(MetadataChoice {
            generation: true,
            ..MetadataChoice::NONE
        });
        assert_eq!(read_export_metadata(&out).generation, meta.generation);
        let without = export(MetadataChoice::REVEALED);
        assert!(read_export_metadata(&without).generation.is_empty());
    }

    /// A TIFF source's metadata is compacted, not the whole file, and still offers every
    /// category it has.
    #[test]
    fn a_tiff_source_is_compacted() {
        let meta = SourceMetadata {
            exif: Some(Arc::from(dirty_exif())),
            generation: Vec::new(),
        };
        let img = pb_encode::ExportImage {
            width: 64,
            height: 64,
            channels: pb_encode::Channels::Rgb,
            // Noise, so LZW leaves the pixel data far larger than the metadata.
            samples: pb_encode::Samples::U8(
                (0..64 * 64 * 3u32)
                    .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
                    .collect(),
            ),
            color: pb_encode::OutputColor::Srgb,
            notes: Vec::new(),
            metadata: meta.output(MetadataChoice::ALL),
        };
        let tiff = pb_encode::tiff::encode_tiff(&img).unwrap();
        let read = read_export_metadata(&tiff);
        let block = read.exif.as_deref().expect("EXIF from the TIFF's own IFDs");
        assert!(
            block.len() < tiff.len() / 4,
            "{} of {}",
            block.len(),
            tiff.len()
        );
        assert_eq!(
            read.available(&pb_encode::ColorDesc::Untagged),
            MetadataChoice {
                generation: false,
                original_profile: false,
                ..MetadataChoice::ALL
            }
        );
        // Olympus's magic number reads the same.
        let mut orf = tiff.clone();
        orf[..4].copy_from_slice(b"IIRO");
        if tiff.starts_with(b"II") {
            assert_eq!(read_export_metadata(&orf).exif, read.exif);
        }
    }

    /// An AVIF's (and on the Mac a HEIC's) `Exif` item is found by the HEIF reader.
    #[test]
    fn heif_exif_items_are_read() {
        let img = pb_encode::ExportImage {
            width: 16,
            height: 16,
            channels: pb_encode::Channels::Rgb,
            samples: pb_encode::Samples::U8(vec![90; 16 * 16 * 3]),
            color: pb_encode::OutputColor::Srgb,
            notes: Vec::new(),
            metadata: OutputMetadata {
                exif: Some(dirty_exif()),
                text: Vec::new(),
            },
        };
        let avif = pb_encode::avif::encode_avif(&img, 50).unwrap();
        let meta = read_export_metadata(&avif);
        assert_eq!(meta.exif.as_deref(), Some(dirty_exif().as_slice()));
        #[cfg(target_os = "macos")]
        {
            let heic = pb_encode::heic::encode_heic(&img, 50).unwrap();
            let meta = read_export_metadata(&heic);
            assert!(meta.available(&pb_encode::ColorDesc::Untagged).camera);
        }
    }

    /// The JPEG XL box walk: the `Exif` box past its offset; nothing for a bare codestream or
    /// a truncated box.
    #[test]
    fn a_jxl_container_exif_box_is_found() {
        let exif = dirty_exif();
        let boxed = |typ: &[u8; 4], body: &[u8]| {
            let mut b = ((8 + body.len()) as u32).to_be_bytes().to_vec();
            b.extend_from_slice(typ);
            b.extend_from_slice(body);
            b
        };
        let mut file = b"\0\0\0\x0CJXL \r\n\x87\n".to_vec();
        file.extend(boxed(b"ftyp", b"jxl \0\0\0\0jxl "));
        let mut payload = 2u32.to_be_bytes().to_vec();
        payload.extend_from_slice(&[0xAA, 0xBB]); // the offset skips these
        payload.extend_from_slice(&exif);
        file.extend(boxed(b"Exif", &payload));
        file.extend(boxed(b"jxlc", &[0xFF, 0x0A, 0, 0]));
        assert_eq!(jxl_exif(&file), Some(exif.as_slice()));
        assert_eq!(
            read_export_metadata(&file).exif.as_deref(),
            Some(exif.as_slice())
        );
        assert_eq!(jxl_exif(&[0xFF, 0x0A, 1, 2, 3]), None);
        let mut truncated = b"\0\0\0\x0CJXL \r\n\x87\n".to_vec();
        truncated.extend_from_slice(&[0, 0, 0, 200, b'E', b'x', b'i', b'f', 0, 0]);
        assert_eq!(jxl_exif(&truncated), None);
    }

    #[test]
    fn garbage_has_no_metadata() {
        for junk in [
            &b""[..],
            b"II*\0",
            b"MM\0*\0\0\0\x08",
            b"\x89PNG\r\n\x1a\n",
            b"RIFF\0\0\0\0WEBP",
        ] {
            assert_eq!(read_export_metadata(junk), SourceMetadata::default());
        }
    }
}
