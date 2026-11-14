//! ICC profile handling for export: the **sanitizer** (export-plan.md § 3.7, review F6) and
//! the few profiles we synthesize.
//!
//! A colour profile is itself a metadata container: its description, copyright and private
//! tags can name a device, a person or a calibration session. Under *Strip metadata* we keep
//! the profile's **colour meaning** exactly — every data tag on an allowlist is copied
//! verbatim, so pixels are never touched — and replace everything else:
//!
//! - the allowlisted data tags (`rXYZ gXYZ bXYZ wtpt bkpt chad chrm rTRC gTRC bTRC kTRC
//!   A2B0..2 B2A0..2 cicp`) are copied byte-for-byte;
//! - `desc` and `cprt` are rewritten with neutral text (a well-known standard name like
//!   "Display P3" survives; anything else becomes a generic label);
//! - the header's CMM, date, platform, manufacturer, model, attributes, creator and profile
//!   ID are zeroed;
//! - every other tag is dropped.
//!
//! Residual risk (documented in the plan): a custom-calibrated profile's *numbers* can still
//! be a weak device fingerprint. That is the price of exact colour.

use std::collections::HashMap;

/// Tags whose bytes define colour and are copied verbatim.
const ALLOWED_DATA_TAGS: &[&[u8; 4]] = &[
    b"rXYZ", b"gXYZ", b"bXYZ", b"wtpt", b"bkpt", b"chad", b"chrm", b"rTRC", b"gTRC", b"bTRC",
    b"kTRC", b"A2B0", b"A2B1", b"A2B2", b"B2A0", b"B2A1", b"B2A2", b"cicp",
];

/// Standard profile descriptions that identify nobody and are kept as-is, so the export
/// still reads "Display P3" in Preview's inspector rather than a generic label.
const KNOWN_DESCRIPTIONS: &[&str] = &[
    "sRGB IEC61966-2.1",
    "sRGB IEC61966-2-1 black scaled",
    "sRGB",
    "sRGB built-in",
    "Display P3",
    "Adobe RGB (1998)",
    "ProPhoto RGB",
    "ROMM-RGB",
    "Rec. 2020",
    "ITU-R BT.2020",
    "Generic Gray Gamma 2.2 Profile",
    "Dot Gain 20%",
    "Gray Gamma 2.2",
];

const COPYRIGHT: &str = "No copyright, use freely";

/// The profile's data colour space (header bytes 16..20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IccSpace {
    Rgb,
    Gray,
    Cmyk,
    Other,
}

/// Why a profile couldn't be sanitized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IccError {
    /// Too short, a size field that lies, or a tag pointing outside the profile.
    Malformed(&'static str),
    /// A device link / abstract / named-colour profile — not embeddable in an image.
    UnsupportedClass,
}

impl std::fmt::Display for IccError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IccError::Malformed(why) => write!(f, "malformed ICC profile: {why}"),
            IccError::UnsupportedClass => write!(f, "ICC profile class can't describe an image"),
        }
    }
}

impl std::error::Error for IccError {}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

/// The data colour space a profile declares, or `None` if it's too short to have a header.
pub fn data_color_space(icc: &[u8]) -> Option<IccSpace> {
    let sig = icc.get(16..20)?;
    Some(match sig {
        b"RGB " => IccSpace::Rgb,
        b"GRAY" => IccSpace::Gray,
        b"CMYK" => IccSpace::Cmyk,
        _ => IccSpace::Other,
    })
}

struct TagEntry {
    sig: [u8; 4],
    offset: usize,
    size: usize,
}

fn parse_tags(icc: &[u8]) -> Result<Vec<TagEntry>, IccError> {
    if icc.len() < 132 {
        return Err(IccError::Malformed("shorter than a header"));
    }
    if &icc[36..40] != b"acsp" {
        return Err(IccError::Malformed("missing 'acsp' signature"));
    }
    let declared = be32(icc, 0).unwrap_or(0) as usize;
    if declared > icc.len() || declared < 132 {
        return Err(IccError::Malformed("size field disagrees with the data"));
    }
    let icc = &icc[..declared];
    let count = be32(icc, 128).unwrap_or(0) as usize;
    let table_end = 132usize
        .checked_add(
            count
                .checked_mul(12)
                .ok_or(IccError::Malformed("tag count"))?,
        )
        .ok_or(IccError::Malformed("tag count"))?;
    if table_end > icc.len() || count > 4096 {
        return Err(IccError::Malformed("tag table runs past the end"));
    }
    let mut tags = Vec::with_capacity(count);
    for i in 0..count {
        let at = 132 + i * 12;
        let mut sig = [0u8; 4];
        sig.copy_from_slice(&icc[at..at + 4]);
        let offset = be32(icc, at + 4).unwrap_or(0) as usize;
        let size = be32(icc, at + 8).unwrap_or(0) as usize;
        let end = offset
            .checked_add(size)
            .ok_or(IccError::Malformed("tag overflows"))?;
        if offset < table_end || end > icc.len() {
            return Err(IccError::Malformed("tag points outside the profile"));
        }
        tags.push(TagEntry { sig, offset, size });
    }
    Ok(tags)
}

/// Read the profile's description text (`desc`, v2 `textDescriptionType` or v4 `mluc`).
pub fn description(icc: &[u8]) -> Option<String> {
    let tags = parse_tags(icc).ok()?;
    let t = tags.iter().find(|t| &t.sig == b"desc")?;
    let data = &icc[t.offset..t.offset + t.size];
    match data.get(0..4)? {
        b"desc" => {
            let n = be32(data, 8)? as usize;
            let s = data.get(12..12 + n)?;
            let s = s.split(|&b| b == 0).next().unwrap_or(&[]);
            Some(String::from_utf8_lossy(s).into_owned())
        }
        b"mluc" => {
            let records = be32(data, 8)? as usize;
            if records == 0 {
                return None;
            }
            let len = be32(data, 20)? as usize;
            let off = be32(data, 24)? as usize;
            let raw = data.get(off..off + len)?;
            let units: Vec<u16> = raw
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            Some(
                String::from_utf16_lossy(&units)
                    .trim_end_matches('\0')
                    .to_string(),
            )
        }
        _ => None,
    }
}

fn pad4(v: &mut Vec<u8>) {
    while !v.len().is_multiple_of(4) {
        v.push(0);
    }
}

/// A v2 `textDescriptionType` (the `desc` type in ICC v2.x).
fn v2_desc(text: &str) -> Vec<u8> {
    let ascii: Vec<u8> = text.bytes().filter(|b| b.is_ascii() && *b != 0).collect();
    let mut v = Vec::new();
    v.extend_from_slice(b"desc");
    v.extend_from_slice(&[0; 4]);
    v.extend_from_slice(&((ascii.len() + 1) as u32).to_be_bytes());
    v.extend_from_slice(&ascii);
    v.push(0);
    v.extend_from_slice(&[0; 4]); // Unicode language code
    v.extend_from_slice(&[0; 4]); // Unicode count
    v.extend_from_slice(&[0; 2]); // ScriptCode code
    v.push(0); // ScriptCode count
    v.extend_from_slice(&[0; 67]); // ScriptCode string
    v
}

/// A v2 `textType` (the `cprt` type in ICC v2.x).
fn v2_text(text: &str) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"text");
    v.extend_from_slice(&[0; 4]);
    v.extend(text.bytes().filter(|b| b.is_ascii() && *b != 0));
    v.push(0);
    v
}

/// A v4 `multiLocalizedUnicodeType` with one en-US record.
fn v4_mluc(text: &str) -> Vec<u8> {
    let utf16: Vec<u8> = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
    let mut v = Vec::new();
    v.extend_from_slice(b"mluc");
    v.extend_from_slice(&[0; 4]);
    v.extend_from_slice(&1u32.to_be_bytes()); // record count
    v.extend_from_slice(&12u32.to_be_bytes()); // record size
    v.extend_from_slice(b"enUS");
    v.extend_from_slice(&(utf16.len() as u32).to_be_bytes());
    v.extend_from_slice(&28u32.to_be_bytes()); // string offset from the tag start
    v.extend_from_slice(&utf16);
    v
}

/// Rebuild `icc` keeping only colour-defining data (see the module docs). Pixels encoded in
/// the original profile mean exactly the same thing under the result.
pub fn sanitize(icc: &[u8]) -> Result<Vec<u8>, IccError> {
    let tags = parse_tags(icc)?;
    match &icc[12..16] {
        b"mntr" | b"scnr" | b"spac" | b"prtr" => {}
        _ => return Err(IccError::UnsupportedClass),
    }
    let v4 = icc[8] >= 4;
    let space = data_color_space(icc).unwrap_or(IccSpace::Other);

    let desc_text = description(icc)
        .map(|d| d.trim().to_string())
        .filter(|d| KNOWN_DESCRIPTIONS.iter().any(|k| k.eq_ignore_ascii_case(d)))
        .unwrap_or_else(|| match space {
            IccSpace::Gray => "Gray profile".to_string(),
            _ => "RGB profile".to_string(),
        });

    // Kept data tags, de-duplicated by their source span so shared curves stay shared.
    let kept: Vec<&TagEntry> = tags
        .iter()
        .filter(|t| ALLOWED_DATA_TAGS.iter().any(|a| **a == t.sig))
        .collect();
    let tag_count = kept.len() + 2; // + desc + cprt
    let table_len = 4 + tag_count * 12;

    let mut data: Vec<u8> = Vec::new();
    let mut table: Vec<([u8; 4], u32, u32)> = Vec::with_capacity(tag_count);
    let base = 128 + table_len;
    let mut placed: HashMap<(usize, usize), u32> = HashMap::new();
    let place = |data: &mut Vec<u8>, bytes: &[u8]| -> u32 {
        pad4(data);
        let at = (base + data.len()) as u32;
        data.extend_from_slice(bytes);
        at
    };
    for t in &kept {
        let at = match placed.get(&(t.offset, t.size)) {
            Some(&at) => at,
            None => {
                let at = place(&mut data, &icc[t.offset..t.offset + t.size]);
                placed.insert((t.offset, t.size), at);
                at
            }
        };
        table.push((t.sig, at, t.size as u32));
    }
    let desc = if v4 {
        v4_mluc(&desc_text)
    } else {
        v2_desc(&desc_text)
    };
    let cprt = if v4 {
        v4_mluc(COPYRIGHT)
    } else {
        v2_text(COPYRIGHT)
    };
    let desc_at = place(&mut data, &desc);
    table.push((*b"desc", desc_at, desc.len() as u32));
    let cprt_at = place(&mut data, &cprt);
    table.push((*b"cprt", cprt_at, cprt.len() as u32));
    pad4(&mut data);

    let mut out = Vec::with_capacity(base + data.len());
    out.extend_from_slice(&icc[..128]);
    // Identity-bearing header fields → zero.
    for range in [4..8, 24..36, 40..44, 48..64, 80..100] {
        out[range].fill(0);
    }
    out.extend_from_slice(&(tag_count as u32).to_be_bytes());
    for (sig, at, size) in &table {
        out.extend_from_slice(sig);
        out.extend_from_slice(&at.to_be_bytes());
        out.extend_from_slice(&size.to_be_bytes());
    }
    out.extend_from_slice(&data);
    let total = out.len() as u32;
    out[0..4].copy_from_slice(&total.to_be_bytes());
    Ok(out)
}

/// A clean Display P3 profile (the tone-mapped-HDR output space, § 3.8).
pub fn display_p3_profile() -> Vec<u8> {
    let encoded = moxcms::ColorProfile::new_display_p3()
        .encode()
        .expect("moxcms encodes its own Display P3 profile");
    sanitize(&encoded).unwrap_or(encoded)
}

/// A clean profile for CICP code points (HEIF/AVIF `nclx` sources). `None` when the code
/// points aren't ones moxcms can describe — the caller tags sRGB instead.
pub fn profile_for_cicp(c: crate::Cicp) -> Option<Vec<u8>> {
    use moxcms::{CicpColorPrimaries, CicpProfile, MatrixCoefficients, TransferCharacteristics};
    let cicp = CicpProfile {
        color_primaries: CicpColorPrimaries::try_from(c.primaries).ok()?,
        transfer_characteristics: TransferCharacteristics::try_from(c.transfer).ok()?,
        matrix_coefficients: MatrixCoefficients::try_from(c.matrix).ok()?,
        full_range: c.full_range,
    };
    // moxcms 0.8.1's `update_rgb_colorimetry_from_cicp` returns `false` even on success, so
    // judge success by its side effects: `cicp` is set only once the primaries and white
    // point resolved, and the curve must convert.
    let _: moxcms::ToneReprCurve = cicp.transfer_characteristics.try_into().ok()?;
    let mut profile = moxcms::ColorProfile::new_srgb();
    profile.cicp = None;
    profile.update_rgb_colorimetry_from_cicp(cicp);
    profile.cicp?;
    let encoded = profile.encode().ok()?;
    sanitize(&encoded).ok()
}

/// Whether `icc` describes plain sRGB closely enough that an `sRGB` chunk says the same
/// thing. Deliberately **tight** (review F6): primaries and white within 0.002, the curve
/// checked at several points — never the display path's loose gamma 1.9–2.5 test.
pub fn is_srgb(icc: &[u8]) -> bool {
    let Ok(p) = moxcms::ColorProfile::new_from_slice(icc) else {
        return false;
    };
    let s = moxcms::ColorProfile::new_srgb();
    let close = |a: moxcms::Xyzd, b: moxcms::Xyzd| {
        (a.x - b.x).abs() < 0.002 && (a.y - b.y).abs() < 0.002 && (a.z - b.z).abs() < 0.002
    };
    if !(close(p.red_colorant, s.red_colorant)
        && close(p.green_colorant, s.green_colorant)
        && close(p.blue_colorant, s.blue_colorant)
        && close(p.white_point, s.white_point))
    {
        return false;
    }
    let (Some(curve), Some(srgb)) = (p.red_trc.as_ref(), s.red_trc.as_ref()) else {
        return false;
    };
    // Every channel must carry the sRGB curve, checked at several points.
    let (Ok(want), Some(g), Some(b)) = (
        srgb.make_linear_evaluator(),
        p.green_trc.as_ref(),
        p.blue_trc.as_ref(),
    ) else {
        return false;
    };
    [curve, g, b].iter().all(|c| {
        let Ok(have) = c.make_linear_evaluator() else {
            return false;
        };
        [0.0f32, 0.02, 0.1, 0.25, 0.5, 0.75, 1.0]
            .iter()
            .all(|&x| (have.evaluate_value(x) - want.evaluate_value(x)).abs() < 0.002)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A v2 RGB profile carrying identifying text in desc/cprt/dmnd/dmdd/a private tag,
    /// plus a populated header — the leak fixture (review F6).
    fn leaky_profile() -> Vec<u8> {
        let p3 = moxcms::ColorProfile::new_display_p3().encode().unwrap();
        let tags = parse_tags(&p3).unwrap();
        // Rebuild with extra identifying tags appended.
        let mut extra: Vec<([u8; 4], Vec<u8>)> = tags
            .iter()
            .map(|t| (t.sig, p3[t.offset..t.offset + t.size].to_vec()))
            .filter(|(s, _)| s != b"desc" && s != b"cprt")
            .collect();
        extra.push((*b"desc", v2_desc("Jane Doe's MacBook display")));
        extra.push((*b"cprt", v2_text("Copyright Jane Doe 2026")));
        extra.push((*b"dmnd", v2_desc("Secret Display Co")));
        extra.push((*b"dmdd", v2_desc("Serial 12345-JANE")));
        extra.push((*b"Priv", b"Priv\0\0\0\0jane@example.com".to_vec()));
        let mut hdr = p3[..128].to_vec();
        hdr[8] = 2; // v2
        hdr[48..52].copy_from_slice(b"JANE");
        hdr[52..56].copy_from_slice(b"MDL1");
        hdr[80..84].copy_from_slice(b"jdoe");
        hdr[24..36].copy_from_slice(&[7; 12]);
        let base = 128 + 4 + extra.len() * 12;
        let mut data = Vec::new();
        let mut table = Vec::new();
        for (sig, bytes) in &extra {
            pad4(&mut data);
            table.push((*sig, (base + data.len()) as u32, bytes.len() as u32));
            data.extend_from_slice(bytes);
        }
        pad4(&mut data);
        let mut out = hdr;
        out.extend_from_slice(&(extra.len() as u32).to_be_bytes());
        for (sig, at, size) in table {
            out.extend_from_slice(&sig);
            out.extend_from_slice(&at.to_be_bytes());
            out.extend_from_slice(&size.to_be_bytes());
        }
        out.extend_from_slice(&data);
        let n = out.len() as u32;
        out[0..4].copy_from_slice(&n.to_be_bytes());
        out
    }

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn sanitize_strips_every_identifying_string_and_header_field() {
        let leaky = leaky_profile();
        assert!(contains(&leaky, b"Jane Doe"), "fixture must actually leak");
        let clean = sanitize(&leaky).unwrap();
        for needle in [
            &b"Jane"[..],
            b"JANE",
            b"jane@example.com",
            b"Secret Display",
            b"Serial 12345",
            b"jdoe",
            b"MDL1",
        ] {
            assert!(
                !contains(&clean, needle),
                "leaked {:?}",
                String::from_utf8_lossy(needle)
            );
        }
        assert!(clean[24..36].iter().all(|&b| b == 0), "date zeroed");
        assert_eq!(description(&clean).as_deref(), Some("RGB profile"));
    }

    #[test]
    fn sanitize_preserves_colorimetry_exactly() {
        let leaky = leaky_profile();
        let clean = sanitize(&leaky).unwrap();
        let a = moxcms::ColorProfile::new_from_slice(&leaky).unwrap();
        let b = moxcms::ColorProfile::new_from_slice(&clean).unwrap();
        assert_eq!(a.red_colorant, b.red_colorant);
        assert_eq!(a.green_colorant, b.green_colorant);
        assert_eq!(a.blue_colorant, b.blue_colorant);
        assert_eq!(a.white_point, b.white_point);
        // Transform a few colours through both — identical output.
        let srgb = moxcms::ColorProfile::new_srgb();
        let opts = moxcms::TransformOptions::default();
        let ta = a
            .create_transform_8bit(moxcms::Layout::Rgb, &srgb, moxcms::Layout::Rgb, opts)
            .unwrap();
        let tb = b
            .create_transform_8bit(moxcms::Layout::Rgb, &srgb, moxcms::Layout::Rgb, opts)
            .unwrap();
        let src: Vec<u8> = (0..=255u8).flat_map(|v| [v, 255 - v, v / 2]).collect();
        let (mut oa, mut ob) = (vec![0u8; src.len()], vec![0u8; src.len()]);
        ta.transform(&src, &mut oa).unwrap();
        tb.transform(&src, &mut ob).unwrap();
        assert_eq!(oa, ob);
    }

    #[test]
    fn known_standard_descriptions_survive() {
        let p3 = display_p3_profile();
        // moxcms names its profile something; we only keep it if it's a known name, so
        // either way the result is a neutral label.
        let d = description(&p3).unwrap();
        assert!(d == "Display P3" || d == "RGB profile", "{d}");
        assert_eq!(data_color_space(&p3), Some(IccSpace::Rgb));
    }

    #[test]
    fn sanitize_is_idempotent_and_v4_works() {
        let p3 = moxcms::ColorProfile::new_display_p3().encode().unwrap();
        let once = sanitize(&p3).unwrap();
        let twice = sanitize(&once).unwrap();
        assert_eq!(once, twice);
        assert!(moxcms::ColorProfile::new_from_slice(&once).is_ok());
    }

    #[test]
    fn malformed_profiles_are_rejected_not_panicked_on() {
        assert!(sanitize(&[]).is_err());
        assert!(sanitize(&[0u8; 200]).is_err());
        let mut lying = display_p3_profile();
        let n = lying.len() as u32 + 1000;
        lying[0..4].copy_from_slice(&n.to_be_bytes());
        assert!(sanitize(&lying).is_err());
        let mut bad_tag = display_p3_profile();
        bad_tag[132 + 4..132 + 8].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(sanitize(&bad_tag).is_err());
    }

    #[test]
    fn cicp_p3_builds_a_p3_profile_and_srgb_detection_is_tight() {
        let p3 = profile_for_cicp(crate::Cicp {
            primaries: 12,
            transfer: 13,
            matrix: 0,
            full_range: true,
        })
        .unwrap();
        assert!(!is_srgb(&p3));
        let srgb = sanitize(&moxcms::ColorProfile::new_srgb().encode().unwrap()).unwrap();
        assert!(is_srgb(&srgb));
        // A gamma-2.2 profile on sRGB primaries is NOT sRGB (the display path's loose test
        // would have said it was — review F6).
        let mut g22 = moxcms::ColorProfile::new_srgb();
        let curve = moxcms::ToneReprCurve::Parametric(vec![2.2]);
        g22.red_trc = Some(curve.clone());
        g22.green_trc = Some(curve.clone());
        g22.blue_trc = Some(curve);
        assert!(!is_srgb(&g22.encode().unwrap()));
    }
}
