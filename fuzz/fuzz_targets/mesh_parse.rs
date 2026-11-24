//! Fuzz the 3D model parsers and the still renderer (task #151): arbitrary bytes, read as STL,
//! OBJ and 3MF, must never panic, hang, or over-allocate. Whatever parses is also rendered,
//! small, so the rasterizer sees hostile geometry too (NaN-adjacent extents, huge spans,
//! degenerate triangles).
//!
//! `pb-decode` runs models under `catch_panics`, but a panic here is still a bug: the
//! renderer runs on the decode pool, and `catch_unwind` is the backstop, not the design.
#![no_main]

use libfuzzer_sys::fuzz_target;
use pb_mesh::{CpuRasterizer, Limits, MeshRasterizer, ModelFormat, RenderOptions};

fuzz_target!(|data: &[u8]| {
    // Small caps keep each input fast and make the cap paths reachable.
    let limits = Limits {
        max_triangles: 200_000,
        max_part_bytes: 16 << 20,
        max_parts: 64,
    };
    for format in [ModelFormat::Stl, ModelFormat::Obj, ModelFormat::ThreeMf] {
        if let Ok(model) = pb_mesh::parse(format, data, &limits) {
            assert!(model.triangles.len() <= limits.max_triangles);
            assert!(model
                .triangles
                .iter()
                .flatten()
                .all(|&i| (i as usize) < model.positions.len()));
            if !model.is_empty() {
                let r = CpuRasterizer.render(&model, &RenderOptions::fit(64, 64));
                if let Ok(r) = r {
                    assert_eq!(r.rgba.len(), (r.width * r.height * 4) as usize);
                    assert!(r.width <= 64 && r.height <= 64);
                }
            }
        }
    }
    let _ = pb_mesh::threemf_thumbnail(data);
});
