//! Wavefront OBJ via `tobj` (MIT, fuzzed upstream).
//!
//! Geometry only for now: `mtllib` names a sibling file the decoder can't reach from bytes,
//! so materials fall back to the default colour until the sibling-file seam lands (#154).
//! Per-vertex colours (the common `v x y z r g b` extension) are honoured.
//!
//! ⚠ `tobj` triangulates polygons as fans, which is wrong for concave faces. Most exporters
//! write triangles or convex quads; ear-clipping concave faces ourselves is #151 follow-up.
//!
//! OBJ is conventionally **Y-up**; it is rotated to Z-up here: `(x, y, z) → (x, −z, y)`, which
//! maps OBJ's "towards the viewer" (+Z) onto our front (−Y).

use crate::{Limits, MeshError, Model, ModelFormat};

pub(crate) fn parse(bytes: &[u8], limits: &Limits) -> Result<Model, MeshError> {
    if bytes.len() as u64 > limits.max_part_bytes {
        return Err(MeshError::TooLarge(format!(
            "{} bytes (limit {})",
            bytes.len(),
            limits.max_part_bytes
        )));
    }
    let opts = tobj::LoadOptions {
        triangulate: true,
        ignore_points: true,
        ignore_lines: true,
        ..Default::default()
    };
    let mut reader = bytes;
    let (models, _materials) =
        tobj::load_obj_buf(&mut reader, &opts, |_| Err(tobj::LoadError::OpenFileFailed))
            .map_err(|e| MeshError::Malformed(format!("OBJ: {e}")))?;

    let mut model = Model::new(ModelFormat::Obj);
    let mut colors: Vec<[u8; 3]> = Vec::new();
    let mut any_color = false;
    for m in &models {
        let mesh = &m.mesh;
        let n = mesh.positions.len() / 3;
        let tris = mesh.indices.len() / 3;
        if model.triangles.len() + tris > limits.max_triangles {
            return Err(MeshError::TooLarge(format!(
                "more than {} triangles",
                limits.max_triangles
            )));
        }
        let base = model.positions.len() as u32;
        model
            .positions
            .extend(mesh.positions.chunks_exact(3).map(|p| [p[0], -p[2], p[1]]));
        let vertex_colors = (mesh.vertex_color.len() == n * 3).then_some(&mesh.vertex_color);
        for t in mesh.indices.chunks_exact(3) {
            // An index past this mesh's vertices is malformed: point it past the end so
            // `sanitize` drops the triangle and counts it.
            let idx = |i: u32| {
                if (i as usize) < n {
                    base + i
                } else {
                    u32::MAX
                }
            };
            model.triangles.push([idx(t[0]), idx(t[1]), idx(t[2])]);
            colors.push(match vertex_colors {
                Some(vc) => {
                    any_color = true;
                    average_color(vc, t, n)
                }
                None => crate::DEFAULT_COLOR,
            });
        }
    }
    if any_color {
        model.triangle_colors = Some(colors);
    }
    model.facts.objects = models.len().max(1) as u32;
    Ok(model)
}

/// The mean of a triangle's three vertex colours (0..1 floats) as sRGB bytes.
fn average_color(vc: &[f32], t: &[u32], n: usize) -> [u8; 3] {
    let mut sum = [0f32; 3];
    for &i in t {
        let i = i as usize;
        if i < n {
            for c in 0..3 {
                sum[c] += vc[i * 3 + c];
            }
        }
    }
    sum.map(|s| ((s / 3.0).clamp(0.0, 1.0) * 255.0).round() as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_default(bytes: &[u8]) -> Result<Model, MeshError> {
        crate::parse(ModelFormat::Obj, bytes, &Limits::default())
    }

    #[test]
    fn triangle_and_quad_with_negative_indices() {
        let src = b"v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nf 1 2 3\nf -4 -3 -2 -1\n";
        let m = parse_default(src).unwrap();
        assert_eq!(m.triangles.len(), 3);
        assert_eq!(m.facts.objects, 1);
    }

    #[test]
    fn y_up_becomes_z_up() {
        let m = parse_default(b"v 0 0 0\nv 1 0 0\nv 0 5 0\nf 1 2 3\n").unwrap();
        let b = m.bounds().unwrap();
        assert_eq!(b.max[2], 5.0, "OBJ +Y is our +Z");
        assert_eq!(b.max[1], 0.0);
    }

    #[test]
    fn vertex_colours_become_triangle_colours() {
        let src = b"v 0 0 0 1 0 0\nv 1 0 0 1 0 0\nv 0 1 0 1 0 0\nf 1 2 3\n";
        let m = parse_default(src).unwrap();
        assert_eq!(m.triangle_colors, Some(vec![[255, 0, 0]]));
    }

    #[test]
    fn mtllib_is_ignored_not_fatal() {
        let src = b"mtllib nowhere.mtl\nusemtl red\nv 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n";
        assert_eq!(parse_default(src).unwrap().triangles.len(), 1);
    }

    #[test]
    fn out_of_range_index_is_dropped() {
        let m = parse_default(b"v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\nf 1 2 9\n");
        // tobj may reject the bad face itself; either way nothing out of range survives.
        if let Ok(m) = m {
            assert!(m
                .triangles
                .iter()
                .flatten()
                .all(|&i| (i as usize) < m.positions.len()));
        }
    }
}
