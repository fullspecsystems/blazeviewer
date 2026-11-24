//! STL, binary and ASCII, hand-rolled (plan § Libraries: the format is trivial, and we need the
//! size pre-flight a crate doesn't give us).
//!
//! Binary: an 80-byte header, a little-endian `u32` triangle count, then 50 bytes per triangle
//! (normal, three vertices, a 2-byte attribute). ASCII: `solid … facet normal … outer loop
//! vertex x y z ×3 endloop endfacet … endsolid`.
//!
//! The stored normals are ignored (they are frequently wrong); the renderer recomputes them.
//! STL has no shared vertices, so the model is a triangle soup with sequential indices.

use crate::{Limits, MeshError, Model, ModelFormat};

const HEADER: usize = 84;
const RECORD: usize = 50;

pub(crate) fn parse(bytes: &[u8], limits: &Limits) -> Result<Model, MeshError> {
    if bytes.len() as u64 > limits.max_part_bytes {
        return Err(MeshError::TooLarge(format!(
            "{} bytes (limit {})",
            bytes.len(),
            limits.max_part_bytes
        )));
    }
    let claimed = claimed_count(bytes);
    // An exact size match is binary even when the header starts with "solid" — exporters
    // that write "solid" into a binary header are common, and it is the classic trap.
    if let Some(count) =
        claimed.filter(|&c| HEADER as u64 + RECORD as u64 * c == bytes.len() as u64)
    {
        return parse_binary(bytes, count as usize, limits);
    }
    if looks_ascii(bytes) {
        return parse_ascii(bytes, limits);
    }
    // A binary file whose count disagrees with its length (truncated, or trailing bytes):
    // read the records that are actually present.
    if claimed.is_some() {
        let present = (bytes.len() - HEADER) / RECORD;
        let count = (claimed.unwrap_or(0) as usize).min(present);
        let mut model = parse_binary(bytes, count, limits)?;
        let missing = claimed.unwrap_or(0).saturating_sub(count as u64);
        model.facts.skipped_triangles += missing;
        return Ok(model);
    }
    Err(MeshError::Malformed("not an STL file".into()))
}

/// The binary header's triangle count, if the file is long enough to have one.
fn claimed_count(bytes: &[u8]) -> Option<u64> {
    let c = bytes.get(80..84)?;
    Some(u32::from_le_bytes([c[0], c[1], c[2], c[3]]) as u64)
}

/// Whether the file reads as ASCII STL: `solid` first, and a `facet` (or `endsolid`) soon after.
fn looks_ascii(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(1024)];
    let trimmed = head.trim_ascii_start();
    if !trimmed
        .get(..5)
        .is_some_and(|s| s.eq_ignore_ascii_case(b"solid"))
    {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    contains(&lower, b"facet") || contains(&lower, b"endsolid")
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn parse_binary(bytes: &[u8], count: usize, limits: &Limits) -> Result<Model, MeshError> {
    if count > limits.max_triangles {
        return Err(MeshError::TooLarge(format!(
            "{count} triangles (limit {})",
            limits.max_triangles
        )));
    }
    let mut model = Model::new(ModelFormat::Stl);
    model.positions.reserve_exact(count * 3);
    model.triangles.reserve_exact(count);
    let f = |b: &[u8], at: usize| f32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
    for rec in bytes[HEADER..].chunks_exact(RECORD).take(count) {
        let base = model.positions.len() as u32;
        for v in 0..3 {
            let at = 12 + v * 12;
            model
                .positions
                .push([f(rec, at), f(rec, at + 4), f(rec, at + 8)]);
        }
        model.triangles.push([base, base + 1, base + 2]);
    }
    model.facts.objects = 1;
    Ok(model)
}

fn parse_ascii(bytes: &[u8], limits: &Limits) -> Result<Model, MeshError> {
    let mut model = Model::new(ModelFormat::Stl);
    let mut tokens = bytes
        .split(|b| b.is_ascii_whitespace())
        .filter(|t| !t.is_empty());
    // One facet loop's vertices; normally three, fan-triangulated if an exporter wrote more.
    let mut loop_verts: Vec<[f32; 3]> = Vec::with_capacity(4);
    let mut solids = 0u32;
    while let Some(tok) = tokens.next() {
        if tok.eq_ignore_ascii_case(b"vertex") {
            let mut v = [0f32; 3];
            for c in &mut v {
                *c = tokens
                    .next()
                    .and_then(|t| std::str::from_utf8(t).ok())
                    .and_then(|s| s.parse::<f32>().ok())
                    .ok_or_else(|| MeshError::Malformed("bad ASCII STL vertex".into()))?;
            }
            loop_verts.push(v);
            if loop_verts.len() > 64 {
                return Err(MeshError::Malformed("facet with too many vertices".into()));
            }
        } else if tok.eq_ignore_ascii_case(b"endloop") {
            flush_loop(&mut model, &mut loop_verts, limits)?;
        } else if tok.eq_ignore_ascii_case(b"solid") {
            solids += 1;
        }
    }
    flush_loop(&mut model, &mut loop_verts, limits)?;
    model.facts.objects = solids.max(1);
    Ok(model)
}

fn flush_loop(
    model: &mut Model,
    verts: &mut Vec<[f32; 3]>,
    limits: &Limits,
) -> Result<(), MeshError> {
    if verts.len() >= 3 {
        for i in 1..verts.len() - 1 {
            if model.triangles.len() >= limits.max_triangles {
                return Err(MeshError::TooLarge(format!(
                    "more than {} triangles",
                    limits.max_triangles
                )));
            }
            let base = model.positions.len() as u32;
            model.positions.extend([verts[0], verts[i], verts[i + 1]]);
            model.triangles.push([base, base + 1, base + 2]);
        }
    } else if !verts.is_empty() {
        model.facts.skipped_triangles += 1;
    }
    verts.clear();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::cube_stl_binary;

    fn parse_default(bytes: &[u8]) -> Result<Model, MeshError> {
        crate::parse(ModelFormat::Stl, bytes, &Limits::default())
    }

    #[test]
    fn binary_cube() {
        let m = parse_default(&cube_stl_binary()).unwrap();
        assert_eq!(m.triangles.len(), 12);
        assert_eq!(m.positions.len(), 36);
        let b = m.bounds().unwrap();
        assert_eq!((b.min, b.max), ([0.0; 3], [1.0; 3]));
    }

    #[test]
    fn binary_with_solid_header_is_still_binary() {
        let mut bytes = cube_stl_binary();
        bytes[..12].copy_from_slice(b"solid facet ");
        let m = parse_default(&bytes).unwrap();
        assert_eq!(m.triangles.len(), 12);
    }

    #[test]
    fn ascii_triangle_and_quad_loop() {
        let text = b"solid t\n facet normal 0 0 1\n  outer loop\n   vertex 0 0 0\n   vertex 1 0 0\n   vertex 0 1 0\n  endloop\n endfacet\n facet normal 0 0 1\n  outer loop\n   vertex 0 0 1\n   vertex 1 0 1\n   vertex 1 1 1\n   vertex 0 1 1\n  endloop\n endfacet\nendsolid t\n";
        let m = parse_default(text).unwrap();
        assert_eq!(
            m.triangles.len(),
            3,
            "one triangle + a quad fanned into two"
        );
    }

    #[test]
    fn truncated_binary_reads_what_is_present() {
        let mut bytes = cube_stl_binary();
        bytes.truncate(HEADER + RECORD * 5 + 7);
        let m = parse_default(&bytes).unwrap();
        assert_eq!(m.triangles.len(), 5);
        assert_eq!(m.facts.skipped_triangles, 7);
    }

    #[test]
    fn lying_count_is_refused_before_allocating() {
        let mut bytes = vec![0u8; HEADER];
        bytes[80..84].copy_from_slice(&u32::MAX.to_le_bytes());
        // Count says 4 billion, file holds zero records: reads zero, allocates nothing big.
        let m = parse_default(&bytes).unwrap();
        assert!(m.is_empty());
        let limits = Limits {
            max_triangles: 3,
            ..Limits::default()
        };
        assert!(matches!(
            parse(&cube_stl_binary(), &limits),
            Err(MeshError::TooLarge(_))
        ));
    }

    #[test]
    fn non_finite_vertices_are_dropped() {
        let mut bytes = cube_stl_binary();
        bytes[HEADER + 12..HEADER + 16].copy_from_slice(&f32::NAN.to_le_bytes());
        let m = parse_default(&bytes).unwrap();
        assert_eq!(m.triangles.len(), 11);
        assert_eq!(m.facts.skipped_triangles, 1);
    }

    #[test]
    fn garbage_is_malformed() {
        assert!(matches!(
            parse_default(b"hello"),
            Err(MeshError::Malformed(_))
        ));
        assert!(matches!(parse_default(b""), Err(MeshError::Malformed(_))));
    }
}
