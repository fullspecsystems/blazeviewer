//! Export metadata (export-plan.md § 3.7): what a source carries that an export may copy
//! ([`SourceMetadata`]), which categories the user chose ([`MetadataChoice`]), and what an
//! encoder embeds ([`OutputMetadata`]).
//!
//! **EXIF is allowlist only.** The source's EXIF is parsed into typed fields; only the tags in
//! [`RULES`] survive, each checked against its expected type and count and, for text, its
//! length and characters. The survivors are written into a **fresh** TIFF structure (IFD0, plus
//! an Exif sub-IFD and a GPS sub-IFD only when something goes in them). The source's bytes are
//! never copied, so nothing off the list (MakerNote, UserComment, serial numbers, the thumbnail
//! IFD, XMP, IPTC, the pixel dimensions, …) can ride along by accident.
//!
//! The pixels are already upright by the time anything is encoded (`prepare` bakes the
//! rotation in), so the fresh block always says Orientation = 1, never the source's value.
//!
//! **Generation data** (AI prompts and workflows, task #137) is PNG text, copied only into PNG
//! output and only for the keywords in [`GENERATION_KEYWORDS`].
//!
//! Pure, like the rest of the crate: no I/O. The parser and writer are kamadak-exif's
//! (BSD-2-Clause), the crate `pb-decode` already reads orientation with.

use std::sync::Arc;

use exif::{Context, Field, In, Tag, Value};

use crate::types::ColorDesc;

/// What the source file carries that an export may copy. Raw, as found: the filtering happens
/// in [`SourceMetadata::output`], never here.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SourceMetadata {
    /// The source's EXIF payload: the TIFF structure, without the `Exif\0\0` prefix. For a
    /// TIFF-structured source (TIFF, most camera RAW), whose "EXIF" is the whole file, it is
    /// instead the allowlisted superset rewritten by [`compact_exif`], so the image data isn't
    /// copied along with it.
    pub exif: Option<Arc<[u8]>>,
    /// PNG text chunks that are AI generation data `(keyword, text)`, e.g. `parameters`,
    /// `prompt`, `workflow`.
    pub generation: Vec<(String, String)>,
}

/// Which categories to copy. Only consulted when `PrepareSpec::strip_metadata` is false.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MetadataChoice {
    /// Capture date and time (with offset), make and model, lens, exposure.
    pub camera: bool,
    /// EXIF Artist and Copyright.
    pub copyright: bool,
    /// The GPS tags.
    pub location: bool,
    /// AI generation text (PNG output only).
    pub generation: bool,
    /// The source's colour profile verbatim instead of the sanitized one.
    pub original_profile: bool,
}

impl MetadataChoice {
    /// Nothing copied.
    pub const NONE: MetadataChoice = MetadataChoice {
        camera: false,
        copyright: false,
        location: false,
        generation: false,
        original_profile: false,
    };

    /// Every category.
    pub const ALL: MetadataChoice = MetadataChoice {
        camera: true,
        copyright: true,
        location: true,
        generation: true,
        original_profile: true,
    };

    /// The defaults when *Strip metadata* is first unchecked (export-plan.md § 3.7): camera
    /// and copyright on; location, generation data and the original profile off.
    pub const REVEALED: MetadataChoice = MetadataChoice {
        camera: true,
        copyright: true,
        location: false,
        generation: false,
        original_profile: false,
    };

    /// The categories on in both (e.g. what the user chose that this photo actually has).
    pub fn and(self, other: MetadataChoice) -> MetadataChoice {
        MetadataChoice {
            camera: self.camera && other.camera,
            copyright: self.copyright && other.copyright,
            location: self.location && other.location,
            generation: self.generation && other.generation,
            original_profile: self.original_profile && other.original_profile,
        }
    }

    /// Whether any category is on.
    pub fn any(self) -> bool {
        self != MetadataChoice::NONE
    }

    fn wants(self, c: Category) -> bool {
        match c {
            Category::Camera => self.camera,
            Category::Copyright => self.copyright,
            Category::Location => self.location,
        }
    }
}

/// What an encoder embeds. Empty under *Strip metadata*.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OutputMetadata {
    /// A freshly written EXIF payload (TIFF structure, no `Exif\0\0`), allowlisted tags only,
    /// always with Orientation = 1.
    pub exif: Option<Vec<u8>>,
    /// PNG text chunks to write `(keyword, text)`. Only PNG output carries them.
    pub text: Vec<(String, String)>,
}

impl OutputMetadata {
    /// Whether there is nothing to embed.
    pub fn is_empty(&self) -> bool {
        self.exif.is_none() && self.text.is_empty()
    }
}

/// The PNG text keywords that are generation data: ComfyUI's `prompt` (the executed graph) and
/// `workflow` (the UI graph), and the Automatic1111 family's `parameters`. The same list
/// `pb-decode`'s reader collects.
pub const GENERATION_KEYWORDS: [&str; 3] = ["prompt", "workflow", "parameters"];

/// The longest generation text copied, in bytes (`pb-decode`'s reader caps a chunk at the same).
pub const MAX_GENERATION_TEXT: usize = 1 << 20;

impl SourceMetadata {
    /// Which categories this source has anything to copy for. `original_profile` comes from
    /// the source's colour, which lives on the `ExportSource`: it is available when the source
    /// embeds an ICC profile (code points and untagged sources have none to keep).
    pub fn available(&self, color: &ColorDesc) -> MetadataChoice {
        let kept = self
            .exif
            .as_deref()
            .map(|raw| allowlisted(raw, MetadataChoice::ALL))
            .unwrap_or_default();
        let has = |c: Category| kept.iter().any(|(k, _)| *k == c);
        MetadataChoice {
            camera: has(Category::Camera),
            copyright: has(Category::Copyright),
            location: has(Category::Location),
            generation: !self.generation_text().is_empty(),
            original_profile: matches!(color, ColorDesc::Icc(_)),
        }
    }

    /// What an encoder embeds for `choice`: a fresh EXIF block of the chosen categories'
    /// allowlisted tags (or none, when nothing survives), and the generation text when chosen.
    /// The caller decides *Strip metadata*; this never looks at it.
    pub fn output(&self, choice: MetadataChoice) -> OutputMetadata {
        let exif = self
            .exif
            .as_deref()
            .filter(|_| choice.camera || choice.copyright || choice.location)
            .and_then(|raw| build_exif(raw, choice));
        let text = if choice.generation {
            self.generation_text()
        } else {
            Vec::new()
        };
        OutputMetadata { exif, text }
    }

    /// The generation chunks worth copying: allowlisted keywords, bounded, no NULs (PNG text
    /// can't hold one), and a repeated keyword dropped entirely (which copy describes the image
    /// is ambiguous; the same rule as `pb-decode`'s reader).
    fn generation_text(&self) -> Vec<(String, String)> {
        let count = |k: &str| self.generation.iter().filter(|(g, _)| g == k).count();
        self.generation
            .iter()
            .filter(|(k, t)| {
                GENERATION_KEYWORDS.contains(&k.as_str())
                    && count(k) == 1
                    && !t.is_empty()
                    && t.len() <= MAX_GENERATION_TEXT
                    && !t.contains('\0')
            })
            .cloned()
            .collect()
    }
}

/// Rewrite a TIFF-structured EXIF payload as just its allowlisted tags (every category), so a
/// TIFF or RAW source (whose "EXIF" is the whole file) can be held without the image data.
/// `None` when nothing on the allowlist is there.
pub fn compact_exif(tiff: &[u8]) -> Option<Vec<u8>> {
    build_exif(tiff, MetadataChoice::ALL)
}

// ---- The allowlist ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Category {
    Camera,
    Copyright,
    Location,
}

/// What an ASCII value may contain.
#[derive(Debug, Clone, Copy)]
enum Text {
    /// Printable text (UTF-8 bytes allowed), no control characters.
    Free,
    /// `YYYY:MM:DD HH:MM:SS`, digits or blanks.
    DateTime,
    /// `YYYY:MM:DD`.
    Date,
    /// `±HH:MM`.
    Offset,
    /// Fractional seconds: 1 to 9 digits.
    Digits,
    /// Exactly one of these characters (a GPS reference such as `N` or `S`).
    OneOf(&'static [u8]),
}

/// The type, count and bounds a tag's value must have to be copied.
#[derive(Debug, Clone, Copy)]
enum Kind {
    /// ASCII: at most `parts` strings, at most `max` bytes in all.
    Ascii {
        text: Text,
        max: usize,
        parts: usize,
    },
    /// 1..=`max` SHORTs (a LONG or BYTE that fits is narrowed).
    Short { max: usize },
    /// Exactly `n` RATIONALs. A zero denominator drops the tag, unless `unknown_ok`, where
    /// 0/0 means "unknown" (LensSpecification).
    Rational { n: usize, unknown_ok: bool },
    /// Exactly `n` SRATIONALs, no zero denominator.
    SRational { n: usize },
    /// Exactly `n` BYTEs.
    Byte { n: usize },
}

/// One allowlisted tag. `needs` names a tag it means nothing without (a latitude without its
/// N/S reference is a wrong fact, which is worse than a missing one).
#[derive(Debug, Clone, Copy)]
struct Rule {
    tag: Tag,
    category: Category,
    kind: Kind,
    needs: Option<Tag>,
}

const fn rule(tag: Tag, category: Category, kind: Kind) -> Rule {
    Rule {
        tag,
        category,
        kind,
        needs: None,
    }
}

const fn paired(tag: Tag, category: Category, kind: Kind, needs: Tag) -> Rule {
    Rule {
        tag,
        category,
        kind,
        needs: Some(needs),
    }
}

const fn ascii(text: Text, max: usize) -> Kind {
    Kind::Ascii {
        text,
        max,
        parts: 1,
    }
}

const DATE_TIME: Kind = ascii(Text::DateTime, 19);
const OFFSET: Kind = ascii(Text::Offset, 6);
const SUBSEC: Kind = ascii(Text::Digits, 9);
const NAME: Kind = ascii(Text::Free, 128);
const SHORT: Kind = Kind::Short { max: 1 };
const RATIONAL: Kind = Kind::Rational {
    n: 1,
    unknown_ok: false,
};
const SRATIONAL: Kind = Kind::SRational { n: 1 };

use Category::{Camera, Copyright, Location};

/// Every tag an export may copy, by category. Anything else is dropped.
const RULES: &[Rule] = &[
    // Camera & date: when, with what, and how it was exposed.
    rule(Tag::DateTime, Camera, DATE_TIME),
    rule(Tag::DateTimeOriginal, Camera, DATE_TIME),
    rule(Tag::DateTimeDigitized, Camera, DATE_TIME),
    rule(Tag::OffsetTime, Camera, OFFSET),
    rule(Tag::OffsetTimeOriginal, Camera, OFFSET),
    rule(Tag::OffsetTimeDigitized, Camera, OFFSET),
    rule(Tag::SubSecTime, Camera, SUBSEC),
    rule(Tag::SubSecTimeOriginal, Camera, SUBSEC),
    rule(Tag::SubSecTimeDigitized, Camera, SUBSEC),
    rule(Tag::Make, Camera, NAME),
    rule(Tag::Model, Camera, NAME),
    rule(Tag::LensMake, Camera, NAME),
    rule(Tag::LensModel, Camera, NAME),
    rule(
        Tag::LensSpecification,
        Camera,
        Kind::Rational {
            n: 4,
            unknown_ok: true,
        },
    ),
    rule(Tag::ExposureTime, Camera, RATIONAL),
    rule(Tag::FNumber, Camera, RATIONAL),
    rule(Tag::ApertureValue, Camera, RATIONAL),
    rule(Tag::MaxApertureValue, Camera, RATIONAL),
    rule(Tag::ShutterSpeedValue, Camera, SRATIONAL),
    rule(Tag::BrightnessValue, Camera, SRATIONAL),
    rule(Tag::ExposureBiasValue, Camera, SRATIONAL),
    rule(Tag::ExposureProgram, Camera, SHORT),
    rule(Tag::ExposureMode, Camera, SHORT),
    rule(Tag::PhotographicSensitivity, Camera, Kind::Short { max: 3 }),
    rule(Tag::SensitivityType, Camera, SHORT),
    rule(Tag::MeteringMode, Camera, SHORT),
    rule(Tag::LightSource, Camera, SHORT),
    rule(Tag::Flash, Camera, SHORT),
    rule(Tag::FocalLength, Camera, RATIONAL),
    rule(Tag::FocalLengthIn35mmFilm, Camera, SHORT),
    rule(Tag::DigitalZoomRatio, Camera, RATIONAL),
    rule(Tag::WhiteBalance, Camera, SHORT),
    rule(Tag::SceneCaptureType, Camera, SHORT),
    rule(Tag::Contrast, Camera, SHORT),
    rule(Tag::Saturation, Camera, SHORT),
    rule(Tag::Sharpness, Camera, SHORT),
    // Copyright & credit.
    rule(Tag::Artist, Copyright, ascii(Text::Free, 256)),
    // Copyright may hold two strings: the photographer's and the editor's.
    rule(
        Tag::Copyright,
        Copyright,
        Kind::Ascii {
            text: Text::Free,
            max: 512,
            parts: 2,
        },
    ),
    // Location: where, how high, when (GPS time) and which way the camera faced.
    paired(
        Tag::GPSLatitudeRef,
        Location,
        ascii(Text::OneOf(b"NS"), 1),
        Tag::GPSLatitude,
    ),
    paired(
        Tag::GPSLatitude,
        Location,
        Kind::Rational {
            n: 3,
            unknown_ok: false,
        },
        Tag::GPSLatitudeRef,
    ),
    paired(
        Tag::GPSLongitudeRef,
        Location,
        ascii(Text::OneOf(b"EW"), 1),
        Tag::GPSLongitude,
    ),
    paired(
        Tag::GPSLongitude,
        Location,
        Kind::Rational {
            n: 3,
            unknown_ok: false,
        },
        Tag::GPSLongitudeRef,
    ),
    paired(
        Tag::GPSAltitudeRef,
        Location,
        Kind::Byte { n: 1 },
        Tag::GPSAltitude,
    ),
    rule(Tag::GPSAltitude, Location, RATIONAL),
    rule(
        Tag::GPSTimeStamp,
        Location,
        Kind::Rational {
            n: 3,
            unknown_ok: false,
        },
    ),
    rule(Tag::GPSDateStamp, Location, ascii(Text::Date, 10)),
    paired(
        Tag::GPSImgDirectionRef,
        Location,
        ascii(Text::OneOf(b"TM"), 1),
        Tag::GPSImgDirection,
    ),
    rule(Tag::GPSImgDirection, Location, RATIONAL),
];

/// The source's typed fields. A strictly valid block parses without copying; a damaged one
/// falls back to the lenient reader, which keeps what it could read.
fn parse(tiff: &[u8]) -> Vec<Field> {
    if let Ok((fields, _)) = exif::parse_exif(tiff) {
        return fields;
    }
    match exif::Reader::new()
        .continue_on_error(true)
        .read_raw(tiff.to_vec())
    {
        Ok(e) => e.fields().cloned().collect(),
        Err(exif::Error::PartialResult(partial)) => {
            let (e, _) = partial.into_inner();
            e.fields().cloned().collect()
        }
        Err(_) => Vec::new(),
    }
}

/// The source's fields that survive the allowlist for `choice`, with their category, values
/// rebuilt from the validated data. Only the primary image's (IFD0 and its sub-IFDs); a tag
/// seen twice there is ambiguous and dropped.
fn allowlisted(raw: &[u8], choice: MetadataChoice) -> Vec<(Category, Field)> {
    let go = || {
        let fields = parse(raw);
        let primary = |tag: Tag| {
            let mut hits = fields
                .iter()
                .filter(move |f| f.ifd_num == In::PRIMARY && f.tag == tag);
            match (hits.next(), hits.next()) {
                (Some(f), None) => Some(f),
                _ => None,
            }
        };
        let mut kept: Vec<(Category, Field)> = RULES
            .iter()
            .filter(|r| choice.wants(r.category))
            .filter_map(|r| {
                let value = sanitize(&primary(r.tag)?.value, r.kind)?;
                Some((
                    r.category,
                    Field {
                        tag: r.tag,
                        ifd_num: In::PRIMARY,
                        value,
                    },
                ))
            })
            .collect();
        // Drop what lost its partner.
        let present: Vec<Tag> = kept.iter().map(|(_, f)| f.tag).collect();
        kept.retain(|(_, f)| {
            RULES
                .iter()
                .find(|r| r.tag == f.tag)
                .and_then(|r| r.needs)
                .is_none_or(|needed| present.contains(&needed))
        });
        kept
    };
    // kamadak-exif parses hostile bytes; a panic in it must cost the metadata, not the export.
    std::panic::catch_unwind(go).unwrap_or_default()
}

/// A fresh EXIF block of `choice`'s allowlisted tags, or `None` when none survive.
fn build_exif(raw: &[u8], choice: MetadataChoice) -> Option<Vec<u8>> {
    let mut fields: Vec<Field> = allowlisted(raw, choice)
        .into_iter()
        .map(|(_, f)| f)
        .collect();
    if fields.is_empty() {
        return None;
    }
    let in_ctx = |fields: &[Field], c: Context| fields.iter().any(|f| f.tag.context() == c);
    // The pixels are upright: say so, whatever the source said.
    let mut synthesized = vec![Field {
        tag: Tag::Orientation,
        ifd_num: In::PRIMARY,
        value: Value::Short(vec![1]),
    }];
    if in_ctx(&fields, Context::Exif) {
        synthesized.push(Field {
            tag: Tag::ExifVersion,
            ifd_num: In::PRIMARY,
            value: Value::Undefined(b"0232".to_vec(), 0),
        });
    }
    if in_ctx(&fields, Context::Gps) {
        synthesized.push(Field {
            tag: Tag::GPSVersionID,
            ifd_num: In::PRIMARY,
            value: Value::Byte(vec![2, 3, 0, 0]),
        });
    }
    fields.extend(synthesized);
    std::panic::catch_unwind(move || {
        let mut w = exif::experimental::Writer::new();
        for f in &fields {
            w.push_field(f);
        }
        let mut out = std::io::Cursor::new(Vec::new());
        // Big-endian ("MM"), as Apple and most cameras write it.
        w.write(&mut out, false).ok()?;
        Some(out.into_inner())
    })
    .ok()
    .flatten()
}

/// `value` rebuilt as `kind` requires, or `None` when it doesn't fit.
fn sanitize(value: &Value, kind: Kind) -> Option<Value> {
    match kind {
        Kind::Ascii { text, max, parts } => {
            let Value::Ascii(strings) = value else {
                return None;
            };
            let kept: Vec<Vec<u8>> = strings
                .iter()
                .map(|s| trim(s))
                .filter(|s| !s.is_empty())
                .take(parts)
                .map(|s| s.to_vec())
                .collect();
            let total: usize = kept.iter().map(|s| s.len()).sum::<usize>() + kept.len();
            if kept.is_empty() || total > max + 1 || !kept.iter().all(|s| text_ok(s, text)) {
                return None;
            }
            Some(Value::Ascii(kept))
        }
        Kind::Short { max } => {
            let v: Vec<u16> = match value {
                Value::Short(v) => v.clone(),
                Value::Long(v) => v
                    .iter()
                    .map(|&x| u16::try_from(x).ok())
                    .collect::<Option<_>>()?,
                Value::Byte(v) => v.iter().map(|&x| x as u16).collect(),
                _ => return None,
            };
            (!v.is_empty() && v.len() <= max).then_some(Value::Short(v))
        }
        Kind::Rational { n, unknown_ok } => {
            let v: Vec<exif::Rational> = match value {
                Value::Rational(v) => v.clone(),
                Value::SRational(v) => v
                    .iter()
                    .map(|r| {
                        Some(exif::Rational {
                            num: u32::try_from(r.num).ok()?,
                            denom: u32::try_from(r.denom).ok()?,
                        })
                    })
                    .collect::<Option<_>>()?,
                _ => return None,
            };
            let valid = |r: &exif::Rational| r.denom != 0 || (unknown_ok && r.num == 0);
            (v.len() == n && v.iter().all(valid)).then_some(Value::Rational(v))
        }
        Kind::SRational { n } => {
            let v: Vec<exif::SRational> = match value {
                Value::SRational(v) => v.clone(),
                Value::Rational(v) => v
                    .iter()
                    .map(|r| {
                        Some(exif::SRational {
                            num: i32::try_from(r.num).ok()?,
                            denom: i32::try_from(r.denom).ok()?,
                        })
                    })
                    .collect::<Option<_>>()?,
                _ => return None,
            };
            (v.len() == n && v.iter().all(|r| r.denom != 0)).then_some(Value::SRational(v))
        }
        Kind::Byte { n } => match value {
            Value::Byte(v) if v.len() == n => Some(Value::Byte(v.clone())),
            _ => None,
        },
    }
}

/// `s` without trailing NULs and surrounding spaces.
fn trim(s: &[u8]) -> &[u8] {
    let end = s
        .iter()
        .rposition(|&b| b != 0 && b != b' ')
        .map_or(0, |i| i + 1);
    let start = s[..end].iter().position(|&b| b != b' ').unwrap_or(end);
    &s[start..end]
}

fn text_ok(s: &[u8], text: Text) -> bool {
    let digit_or_blank = |b: u8| b.is_ascii_digit() || b == b' ';
    // `pattern`: `d` a digit (or blank, for an unknown part), any other byte itself.
    let shaped = |pattern: &[u8]| {
        s.len() == pattern.len()
            && s.iter().zip(pattern).all(|(&b, &p)| match p {
                b'd' => digit_or_blank(b),
                _ => b == p,
            })
            && s.iter().any(|b| b.is_ascii_digit())
    };
    match text {
        Text::Free => s.iter().all(|&b| b >= 0x20 && b != 0x7F),
        Text::DateTime => shaped(b"dddd:dd:dd dd:dd:dd"),
        Text::Date => shaped(b"dddd:dd:dd"),
        Text::Offset => {
            matches!(s.first(), Some(b'+' | b'-'))
                && s.len() == 6
                && s[1..].iter().zip(b"dd:dd").all(|(&b, &p)| match p {
                    b'd' => b.is_ascii_digit(),
                    _ => b == p,
                })
        }
        Text::Digits => !s.is_empty() && s.len() <= 9 && s.iter().all(u8::is_ascii_digit),
        Text::OneOf(set) => s.len() == 1 && set.contains(&s[0]),
    }
}

/// Read a TIFF-structured EXIF payload's primary-image fields, for the encoders that re-emit
/// the block in their own structure (TIFF, HEIC).
#[cfg(any(feature = "tiff", all(feature = "heic", target_os = "macos")))]
pub(crate) fn fields(tiff: &[u8]) -> Vec<Field> {
    std::panic::catch_unwind(|| parse(tiff))
        .unwrap_or_default()
        .into_iter()
        .filter(|f| f.ifd_num == In::PRIMARY)
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use exif::experimental::Writer;
    use exif::{Rational, SRational};

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

    /// A camera file's EXIF with everything an export must never leak beside what it may keep:
    /// GPS, a MakerNote, serial numbers, a UserComment, an owner name, a unique ID, Orientation
    /// 6, pixel dimensions, an unlisted description, and a thumbnail IFD.
    pub(crate) fn dirty_exif() -> Vec<u8> {
        let fields = vec![
            field(Tag::Make, ascii("Canon")),
            field(Tag::Model, ascii("Canon EOS R5")),
            field(Tag::DateTime, ascii("2024:05:06 07:08:09")),
            field(Tag::Artist, ascii("Jane Doe")),
            field(
                Tag::Copyright,
                Value::Ascii(vec![b"(c) Jane Doe".to_vec(), b"Editor".to_vec()]),
            ),
            field(Tag::Orientation, Value::Short(vec![6])),
            field(Tag::ImageDescription, ascii("my secret description")),
            field(Tag::Software, ascii("Secret Editor 9")),
            field(Tag::DateTimeOriginal, ascii("2024:05:06 07:08:09")),
            field(Tag::OffsetTimeOriginal, ascii("-06:00")),
            field(Tag::SubSecTimeOriginal, ascii("042")),
            field(Tag::ExposureTime, Value::Rational(vec![rat(1, 250)])),
            field(Tag::FNumber, Value::Rational(vec![rat(28, 10)])),
            field(Tag::PhotographicSensitivity, Value::Short(vec![400])),
            field(
                Tag::ExposureBiasValue,
                Value::SRational(vec![SRational { num: -1, denom: 3 }]),
            ),
            field(Tag::Flash, Value::Short(vec![16])),
            field(Tag::FocalLength, Value::Rational(vec![rat(50, 1)])),
            field(
                Tag::LensSpecification,
                Value::Rational(vec![rat(24, 1), rat(70, 1), rat(28, 10), rat(0, 0)]),
            ),
            field(Tag::LensModel, ascii("RF24-70mm F2.8 L IS USM")),
            field(
                Tag::MakerNote,
                Value::Undefined(b"CANON-SECRETS".to_vec(), 0),
            ),
            field(
                Tag::UserComment,
                Value::Undefined(b"ASCII\0\0\0my private note".to_vec(), 0),
            ),
            field(Tag::BodySerialNumber, ascii("SN-123456789")),
            field(Tag::LensSerialNumber, ascii("LSN-987")),
            field(Tag::CameraOwnerName, ascii("Jane Q. Owner")),
            field(
                Tag::ImageUniqueID,
                ascii("0123456789abcdef0123456789abcdef"),
            ),
            field(Tag::PixelXDimension, Value::Long(vec![8192])),
            field(Tag::PixelYDimension, Value::Long(vec![5464])),
            field(Tag::ColorSpace, Value::Short(vec![1])),
            field(Tag::GPSLatitudeRef, ascii("N")),
            field(
                Tag::GPSLatitude,
                Value::Rational(vec![rat(53, 1), rat(32, 1), rat(1234, 100)]),
            ),
            field(Tag::GPSLongitudeRef, ascii("W")),
            field(
                Tag::GPSLongitude,
                Value::Rational(vec![rat(113, 1), rat(29, 1), rat(5, 1)]),
            ),
            field(Tag::GPSAltitudeRef, Value::Byte(vec![0])),
            field(Tag::GPSAltitude, Value::Rational(vec![rat(645, 1)])),
            field(Tag::GPSDateStamp, ascii("2024:05:06")),
            // Not on the allowlist even with Location on.
            field(Tag::GPSDestBearing, Value::Rational(vec![rat(9, 1)])),
        ];
        // The thumbnail IFD carries its own Make: it must not be what survives.
        let thumb = Field {
            tag: Tag::Make,
            ifd_num: In::THUMBNAIL,
            value: ascii("THUMBNAIL-MAKE"),
        };
        let mut w = Writer::new();
        for f in &fields {
            w.push_field(f);
        }
        w.push_field(&thumb);
        let mut out = std::io::Cursor::new(Vec::new());
        w.write(&mut out, true).unwrap();
        out.into_inner()
    }

    pub(crate) fn dirty_source() -> SourceMetadata {
        SourceMetadata {
            exif: Some(Arc::from(dirty_exif())),
            generation: vec![("parameters".into(), "a cat, Steps: 20".into())],
        }
    }

    /// The primary-image fields of a written block, as `(tag, display value)`.
    pub(crate) fn tags(tiff: &[u8]) -> Vec<(Tag, String)> {
        let (fields, _) = exif::parse_exif(tiff).expect("a valid EXIF block");
        assert!(
            fields.iter().all(|f| f.ifd_num == In::PRIMARY),
            "no thumbnail IFD"
        );
        fields
            .into_iter()
            .map(|f| (f.tag, f.display_value().to_string()))
            .collect()
    }

    pub(crate) fn has(tags: &[(Tag, String)], tag: Tag) -> bool {
        tags.iter().any(|(t, _)| *t == tag)
    }

    pub(crate) fn value(tags: &[(Tag, String)], tag: Tag) -> Option<&str> {
        tags.iter()
            .find(|(t, _)| *t == tag)
            .map(|(_, v)| v.as_str())
    }

    /// Check a block that should carry camera + copyright (and location when `gps`), and never
    /// any of the leak fixture's private tags. Shared by the per-format round trips.
    pub(crate) fn assert_clean(tiff: &[u8], gps: bool) {
        let t = tags(tiff);
        assert_eq!(value(&t, Tag::Make), Some("\"Canon\""));
        assert_eq!(value(&t, Tag::Model), Some("\"Canon EOS R5\""));
        assert_eq!(value(&t, Tag::Artist), Some("\"Jane Doe\""));
        assert!(has(&t, Tag::DateTimeOriginal));
        assert_eq!(
            value(&t, Tag::Orientation),
            Some("row 0 at top and column 0 at left")
        );
        for private in [
            Tag::MakerNote,
            Tag::UserComment,
            Tag::BodySerialNumber,
            Tag::LensSerialNumber,
            Tag::CameraOwnerName,
            Tag::ImageUniqueID,
            Tag::ImageDescription,
            Tag::Software,
            Tag::PixelXDimension,
            Tag::PixelYDimension,
            Tag::ColorSpace,
            Tag::GPSDestBearing,
        ] {
            assert!(!has(&t, private), "{private} leaked");
        }
        assert_eq!(has(&t, Tag::GPSLatitude), gps);
        assert_eq!(
            has(&t, Tag::GPSVersionID),
            gps,
            "the GPS IFD only with location"
        );
        let text = String::from_utf8_lossy(tiff);
        for secret in [
            "CANON-SECRETS",
            "private note",
            "SN-123",
            "Owner",
            "secret",
            "THUMBNAIL",
            "Secret Editor",
        ] {
            assert!(!text.contains(secret), "{secret} leaked");
        }
    }

    /// Required test 1, at the metadata level: camera + copyright keeps exactly those, says
    /// Orientation 1, and leaks nothing private; location adds the GPS IFD.
    #[test]
    fn the_allowlist_keeps_what_was_chosen_and_nothing_private() {
        let src = dirty_source();
        let out = src.output(MetadataChoice::REVEALED);
        let exif = out.exif.expect("camera + copyright survive");
        assert_clean(&exif, false);
        let t = tags(&exif);
        assert_eq!(value(&t, Tag::FNumber), Some("2.8"));
        assert_eq!(value(&t, Tag::OffsetTimeOriginal), Some("\"-06:00\""));
        assert!(has(&t, Tag::LensSpecification), "0/0 is an allowed unknown");
        assert!(has(&t, Tag::ExifVersion));
        assert!(!has(&t, Tag::GPSVersionID));
        assert!(out.text.is_empty(), "generation data not chosen");

        let with_gps = src.output(MetadataChoice {
            location: true,
            ..MetadataChoice::REVEALED
        });
        let exif = with_gps.exif.unwrap();
        assert_clean(&exif, true);
        let t = tags(&exif);
        assert_eq!(value(&t, Tag::GPSLatitudeRef), Some("N"));
        assert_eq!(value(&t, Tag::GPSAltitude), Some("645"));
        assert_eq!(value(&t, Tag::GPSVersionID), Some("2.3.0.0"));
    }

    #[test]
    fn categories_are_independent() {
        let src = dirty_source();
        let copyright_only = src
            .output(MetadataChoice {
                copyright: true,
                ..MetadataChoice::NONE
            })
            .exif
            .unwrap();
        let t = tags(&copyright_only);
        assert!(has(&t, Tag::Artist) && has(&t, Tag::Copyright));
        assert!(!has(&t, Tag::Make) && !has(&t, Tag::DateTimeOriginal));
        assert!(
            !has(&t, Tag::ExifIFDPointer),
            "no Exif IFD when nothing goes in it"
        );
        assert_eq!(
            value(&t, Tag::Copyright),
            Some("\"(c) Jane Doe\", \"Editor\"")
        );

        let location_only = src
            .output(MetadataChoice {
                location: true,
                ..MetadataChoice::NONE
            })
            .exif
            .unwrap();
        let t = tags(&location_only);
        assert!(has(&t, Tag::GPSLongitude) && !has(&t, Tag::Artist) && !has(&t, Tag::Make));

        // Nothing chosen, or only things without EXIF: no block at all.
        assert_eq!(src.output(MetadataChoice::NONE), OutputMetadata::default());
        assert_eq!(
            src.output(MetadataChoice {
                original_profile: true,
                ..MetadataChoice::NONE
            }),
            OutputMetadata::default()
        );
    }

    /// Required test 4.
    #[test]
    fn available_reports_what_the_source_has() {
        let p3 = ColorDesc::Icc(Arc::from(crate::icc::display_p3_profile()));
        assert_eq!(dirty_source().available(&p3), MetadataChoice::ALL);
        assert_eq!(
            dirty_source().available(&ColorDesc::Untagged),
            MetadataChoice {
                original_profile: false,
                ..MetadataChoice::ALL
            }
        );
        assert_eq!(
            SourceMetadata::default().available(&ColorDesc::Untagged),
            MetadataChoice::NONE
        );
        // Only private tags: nothing to offer.
        let private = {
            let f = [
                field(Tag::MakerNote, Value::Undefined(vec![1, 2, 3], 0)),
                field(Tag::BodySerialNumber, ascii("123")),
                field(Tag::Orientation, Value::Short(vec![6])),
            ];
            let mut w = Writer::new();
            f.iter().for_each(|f| w.push_field(f));
            let mut out = std::io::Cursor::new(Vec::new());
            w.write(&mut out, false).unwrap();
            out.into_inner()
        };
        let src = SourceMetadata {
            exif: Some(Arc::from(private)),
            generation: vec![("Software".into(), "not generation data".into())],
        };
        assert_eq!(src.available(&ColorDesc::Untagged), MetadataChoice::NONE);
        assert_eq!(src.output(MetadataChoice::ALL), OutputMetadata::default());
        // Copyright only.
        let artist = {
            let f = [field(Tag::Artist, ascii("Someone"))];
            let mut w = Writer::new();
            f.iter().for_each(|f| w.push_field(f));
            let mut out = std::io::Cursor::new(Vec::new());
            w.write(&mut out, false).unwrap();
            out.into_inner()
        };
        let src = SourceMetadata {
            exif: Some(Arc::from(artist)),
            ..SourceMetadata::default()
        };
        assert_eq!(
            src.available(&ColorDesc::Untagged),
            MetadataChoice {
                copyright: true,
                ..MetadataChoice::NONE
            }
        );
        // Garbage EXIF is just nothing.
        let junk = SourceMetadata {
            exif: Some(Arc::from(&b"MM\0*garbage"[..])),
            ..SourceMetadata::default()
        };
        assert_eq!(junk.available(&ColorDesc::Untagged), MetadataChoice::NONE);
        assert_eq!(junk.output(MetadataChoice::ALL).exif, None);
    }

    /// Values of the wrong type, count or shape are dropped, not passed through.
    #[test]
    fn values_are_typed_and_bounded() {
        let long = "x".repeat(200);
        let f = [
            field(Tag::Make, ascii(&long)),                           // too long
            field(Tag::Model, Value::Short(vec![1])),                 // wrong type
            field(Tag::DateTimeOriginal, ascii("yesterday at noon")), // not a date
            field(Tag::DateTime, ascii("2024:01:02 03:04:05")),
            field(Tag::FNumber, Value::Rational(vec![rat(1, 0)])), // zero denominator
            field(
                Tag::ExposureTime,
                Value::Rational(vec![rat(1, 2), rat(1, 3)]),
            ), // count
            field(Tag::Flash, Value::Long(vec![70_000])),          // doesn't fit a SHORT
            field(Tag::MeteringMode, Value::Long(vec![5])),        // fits: narrowed
            field(Tag::Artist, ascii("Line\nbreak")),              // control character
            field(Tag::GPSLatitude, Value::Rational(vec![rat(1, 1); 3])), // no N/S
            field(Tag::GPSLongitudeRef, ascii("X")),               // not E/W
            field(Tag::GPSLongitude, Value::Rational(vec![rat(1, 1); 3])),
            field(Tag::GPSAltitude, Value::Rational(vec![rat(10, 1)])), // ref is optional
        ];
        let mut w = Writer::new();
        f.iter().for_each(|f| w.push_field(f));
        let mut out = std::io::Cursor::new(Vec::new());
        w.write(&mut out, false).unwrap();
        let src = SourceMetadata {
            exif: Some(Arc::from(out.into_inner())),
            ..SourceMetadata::default()
        };
        let t = tags(&src.output(MetadataChoice::ALL).exif.unwrap());
        let kept: Vec<Tag> = t.iter().map(|(t, _)| *t).collect();
        for gone in [
            Tag::Make,
            Tag::Model,
            Tag::DateTimeOriginal,
            Tag::FNumber,
            Tag::ExposureTime,
            Tag::Flash,
            Tag::Artist,
            Tag::GPSLatitude,
            Tag::GPSLongitudeRef,
            Tag::GPSLongitude,
        ] {
            assert!(!kept.contains(&gone), "{gone} should be dropped: {kept:?}");
        }
        assert!(kept.contains(&Tag::DateTime));
        assert_eq!(value(&t, Tag::MeteringMode), Some("pattern"));
        assert!(kept.contains(&Tag::GPSAltitude));
    }

    #[test]
    fn generation_text_is_allowlisted() {
        let src = SourceMetadata {
            exif: None,
            generation: vec![
                ("prompt".into(), "{\"1\": {}}".into()),
                ("Software".into(), "nope".into()),
                ("workflow".into(), "first".into()),
                ("workflow".into(), "second".into()),
                ("parameters".into(), "has\0nul".into()),
            ],
        };
        assert_eq!(
            src.output(MetadataChoice {
                generation: true,
                ..MetadataChoice::NONE
            })
            .text,
            vec![("prompt".to_string(), "{\"1\": {}}".to_string())]
        );
        assert!(src.output(MetadataChoice::REVEALED).text.is_empty());
        assert!(src.available(&ColorDesc::Untagged).generation);
    }

    /// A TIFF-structured source compacts to its allowlisted superset: rereading it gives the
    /// same output for every choice as the original would.
    #[test]
    fn compacting_keeps_every_category_and_nothing_else() {
        let raw = dirty_exif();
        let compact = compact_exif(&raw).unwrap();
        assert!(compact.len() < raw.len());
        let a = dirty_source();
        let b = SourceMetadata {
            exif: Some(Arc::from(compact)),
            ..a.clone()
        };
        for choice in [
            MetadataChoice::REVEALED,
            MetadataChoice::ALL,
            MetadataChoice {
                location: true,
                ..MetadataChoice::NONE
            },
        ] {
            assert_eq!(a.output(choice), b.output(choice), "{choice:?}");
        }
        assert_eq!(compact_exif(b"II*\0\x08\0\0\0\0\0"), None);
    }

    #[test]
    fn choice_helpers() {
        assert!(!MetadataChoice::NONE.any());
        assert!(MetadataChoice::REVEALED.any());
        assert_eq!(MetadataChoice::default(), MetadataChoice::NONE);
        assert_eq!(
            MetadataChoice::ALL.and(MetadataChoice::REVEALED),
            MetadataChoice::REVEALED
        );
        assert!(OutputMetadata::default().is_empty());
    }

    #[test]
    fn text_shapes() {
        assert!(text_ok(b"2024:01:02 03:04:05", Text::DateTime));
        assert!(
            text_ok(b"2024:01:02   :  :  ", Text::DateTime),
            "unknown time"
        );
        assert!(
            !text_ok(b"    :  :     :  :  ", Text::DateTime),
            "all blank"
        );
        assert!(!text_ok(b"2024-01-02 03:04:05", Text::DateTime));
        assert!(text_ok(b"+05:30", Text::Offset));
        assert!(!text_ok(b"05:30", Text::Offset));
        assert!(text_ok(b"123", Text::Digits));
        assert!(!text_ok(b"1234567890", Text::Digits));
        assert!(text_ok("Café ©".as_bytes(), Text::Free));
        assert_eq!(trim(b"  Canon \0\0"), b"Canon");
        assert_eq!(trim(b"\0\0"), b"");
    }
}
