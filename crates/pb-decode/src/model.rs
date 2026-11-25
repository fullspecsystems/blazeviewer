//! **3D models** (task #151): STL / OBJ / 3MF decode to a still through `pb-mesh` — parsed
//! from the item's bytes, then rendered on the CPU at the display fit, like an SVG.
//!
//! Routed by **extension** in `decode_named_bytes_inner` (a 3MF is a ZIP and STL/OBJ are
//! arbitrary bytes, so nothing here can be content-sniffed). The app treats the result as an
//! ordinary image, which is what makes prefetch, blaze, the thumbnail strip, archives (a ZIP
//! of STLs), Copy, Export and Describe work unchanged.
//!
//! When a 3MF's mesh can't be drawn — none present (a sliced `.gcode.3mf`), over the size
//! limits, or malformed — its embedded slicer thumbnail is shown instead (owner decision D2).

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use pb_mesh::{CpuRasterizer, Limits, MeshError, MeshRasterizer, ModelFormat, RenderOptions};

use crate::{common, DecodeError, DecodeRequest, DecodedImage, FitBox};

/// The viewer's model colour, packed `0x00RR_GGBB`: the colour of a model whose file colours
/// nothing. Process-wide, like the chrome accent, because it is one user preference read by
/// every decode worker; the app sets it from Settings ▸ Appearance.
static MODEL_COLOR: AtomicU32 = AtomicU32::new(pack(pb_mesh::DEFAULT_COLOR));

const fn pack([r, g, b]: [u8; 3]) -> u32 {
    (r as u32) << 16 | (g as u32) << 8 | b as u32
}

/// Moves whenever a setting that changes decoded pixels does (today only the model colour).
static LOOK_GEN: AtomicU64 = AtomicU64::new(0);

/// Set the colour models render in when their file doesn't colour them. Stills decoded before
/// the change keep the old colour; the caller re-decodes what it holds. Setting the colour it
/// already is changes nothing, the [look generation](look_generation) included.
pub fn set_model_color(rgb: [u8; 3]) {
    if MODEL_COLOR.swap(pack(rgb), Ordering::AcqRel) != pack(rgb) {
        LOOK_GEN.fetch_add(1, Ordering::Release);
    }
}

/// The generation of the process-wide settings that change decoded pixels (see
/// [`set_model_color`]). Read it **before** decoding: a decode stamped with a generation older
/// than the latest change may have been drawn under the old look, and one stamped with the
/// latest was not (the setting is stored before the generation moves).
pub fn look_generation() -> u64 {
    LOOK_GEN.load(Ordering::Acquire)
}

/// The current model colour (see [`set_model_color`]).
pub fn model_color() -> [u8; 3] {
    let v = MODEL_COLOR.load(Ordering::Relaxed);
    [(v >> 16) as u8, (v >> 8) as u8, v as u8]
}

/// The render's long edge when no fit box is given. A model has no native resolution, so its
/// "natural size" is a nominal render size, as with an SVG.
const NOMINAL_EDGE: u32 = 2048;

pub(crate) fn decode_model(
    format: ModelFormat,
    req: &DecodeRequest,
) -> Result<DecodedImage, DecodeError> {
    let codec = format.codec_name();
    let model = match pb_mesh::parse(format, req.bytes, &Limits::default()) {
        Ok(model) if !model.is_empty() => model,
        Ok(model) => {
            return match model.facts.thumbnail {
                Some(thumb) => decode_thumbnail(&thumb, codec, req.fit),
                None => Err(DecodeError::Corrupt(MeshError::Empty.to_string())),
            };
        }
        Err(e) => {
            let thumb = (format == ModelFormat::ThreeMf)
                .then(|| pb_mesh::threemf_thumbnail(req.bytes))
                .flatten();
            return match thumb {
                Some(thumb) => decode_thumbnail(&thumb, codec, req.fit),
                None => Err(DecodeError::Corrupt(e.to_string())),
            };
        }
    };
    let (w, h) = req.fit.map_or((NOMINAL_EDGE, NOMINAL_EDGE), |f| {
        (f.max_width, f.max_height)
    });
    let opts = RenderOptions {
        base_color: model_color(),
        ..RenderOptions::fit(w, h)
    };
    let still = CpuRasterizer
        .render(&model, &opts)
        .map_err(|e| DecodeError::Corrupt(e.to_string()))?;
    // Rendered at the fit already: no decode-to-fit pass, no EXIF.
    common::finalize(
        still.rgba,
        still.width,
        still.height,
        None,
        codec,
        None,
        false,
    )
}

/// A model as the live 3D view shows it: `frame` (the user's camera, from
/// `pb_mesh::LiveView::frame` for a `width`×`height` viewport) rendered at that size in the
/// current model colour, on a transparent background. What Export, Copy and Describe capture
/// of a model the user has turned (task #153). `name` picks the format, as for the still.
pub fn decode_model_view(
    name: &str,
    bytes: &[u8],
    frame: &pb_mesh::LiveFrame,
    width: u32,
    height: u32,
) -> Result<DecodedImage, DecodeError> {
    let format = std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .and_then(ModelFormat::from_extension)
        .ok_or(DecodeError::Unsupported)?;
    let model = pb_mesh::parse(format, bytes, &Limits::default())
        .map_err(|e| DecodeError::Corrupt(e.to_string()))?;
    let view = CpuRasterizer
        .render_view(&model, frame, width, height, model_color())
        .map_err(|e| DecodeError::Corrupt(e.to_string()))?;
    common::finalize(
        view.rgba,
        view.width,
        view.height,
        None,
        format.codec_name(),
        None,
        false,
    )
}

/// The embedded slicer thumbnail (PNG or JPEG), content-sniffed through the ordinary image
/// path, labelled with the model's codec so the panel says "3MF", not "PNG".
fn decode_thumbnail(
    thumb: &[u8],
    codec: &'static str,
    fit: Option<FitBox>,
) -> Result<DecodedImage, DecodeError> {
    let mut img = crate::decode_bytes_inner(thumb, fit, false)?;
    img.codec = codec;
    Ok(img)
}

#[cfg(test)]
mod tests {
    use crate::{decode_named_bytes, is_model_extension, is_supported_extension, FitBox};

    /// A unit cube as binary STL.
    fn cube_stl() -> Vec<u8> {
        let v = |x: f32, y: f32, z: f32| [x, y, z];
        let quads = [
            [v(0., 0., 0.), v(0., 1., 0.), v(1., 1., 0.), v(1., 0., 0.)],
            [v(0., 0., 1.), v(1., 0., 1.), v(1., 1., 1.), v(0., 1., 1.)],
            [v(0., 0., 0.), v(1., 0., 0.), v(1., 0., 1.), v(0., 0., 1.)],
            [v(0., 1., 0.), v(0., 1., 1.), v(1., 1., 1.), v(1., 1., 0.)],
            [v(0., 0., 0.), v(0., 0., 1.), v(0., 1., 1.), v(0., 1., 0.)],
            [v(1., 0., 0.), v(1., 1., 0.), v(1., 1., 1.), v(1., 0., 1.)],
        ];
        let tris: Vec<[[f32; 3]; 3]> = quads
            .iter()
            .flat_map(|q| [[q[0], q[1], q[2]], [q[0], q[2], q[3]]])
            .collect();
        let mut out = vec![0u8; 80];
        out.extend_from_slice(&(tris.len() as u32).to_le_bytes());
        for t in &tris {
            out.extend_from_slice(&[0u8; 12]);
            for p in t {
                for c in p {
                    out.extend_from_slice(&c.to_le_bytes());
                }
            }
            out.extend_from_slice(&[0, 0]);
        }
        out
    }

    #[test]
    fn model_extensions_are_supported_and_flagged() {
        for ext in ["stl", "STL", "obj", "3mf"] {
            assert!(is_supported_extension(ext), "{ext}");
            assert!(is_model_extension(ext), "{ext}");
        }
        assert!(!is_model_extension("png"));
    }

    #[test]
    fn stl_routes_by_name_and_renders_within_the_fit() {
        let fit = FitBox {
            max_width: 320,
            max_height: 200,
        };
        let img = decode_named_bytes("parts/cube.stl", &cube_stl(), Some(fit), true).unwrap();
        assert_eq!(img.codec, "STL");
        assert!(img.width <= 320 && img.height <= 200);
        assert!(img.is_well_formed());
        assert!(!img.is_preview);
    }

    #[test]
    fn the_model_colour_setting_reaches_the_still() {
        let fit = FitBox {
            max_width: 64,
            max_height: 64,
        };
        let before = super::look_generation();
        super::set_model_color([30, 200, 40]);
        let changed = super::look_generation();
        super::set_model_color([30, 200, 40]);
        let again = super::look_generation();
        let img = decode_named_bytes("cube.stl", &cube_stl(), Some(fit), false);
        super::set_model_color(pb_mesh::DEFAULT_COLOR); // process-wide: put it back first
        assert!(changed > before, "a new colour is a new look");
        assert_eq!(again, changed, "the same colour again is not");
        let img = img.unwrap();
        let at = ((img.height / 2 * img.width + img.width / 2) * 4) as usize;
        let [r, g, b] = [img.pixels[at], img.pixels[at + 1], img.pixels[at + 2]];
        assert!(g > r * 2 && g > b * 2, "green: {r} {g} {b}");
    }

    #[test]
    fn a_turned_view_renders_at_the_windows_size() {
        let model = pb_mesh::parse(
            pb_mesh::ModelFormat::Stl,
            &cube_stl(),
            &pb_mesh::Limits::default(),
        )
        .unwrap();
        let mut view = pb_mesh::LiveView::new(&model).unwrap();
        view.orbit_by(60.0, 20.0, [640.0, 360.0]);
        let frame = view.frame([640.0, 360.0]);
        let img =
            super::decode_model_view("parts/cube.stl", &cube_stl(), &frame, 640, 360).unwrap();
        assert_eq!((img.width, img.height), (640, 360));
        assert_eq!(img.codec, "STL");
        assert!(img.is_well_formed() && !img.is_preview);
        assert_eq!(img.pixels[3], 0, "a transparent background");
        assert!(super::decode_model_view("x.png", &cube_stl(), &frame, 8, 8).is_err());
    }

    #[test]
    fn no_fit_renders_at_the_nominal_size() {
        let img = decode_named_bytes("cube.stl", &cube_stl(), None, false).unwrap();
        assert!(img.width.max(img.height) <= super::NOMINAL_EDGE);
        assert!(img.width.max(img.height) >= super::NOMINAL_EDGE - 1);
    }

    /// Export decodes through its own entry point, not `decode_named_bytes`, so a model needs
    /// routing there too. A model the user hasn't turned exports the still that Copy takes
    /// and the panel sizes: the nominal render, transparent around the model.
    #[test]
    fn an_unturned_model_exports_its_nominal_still() {
        let shown = decode_named_bytes("parts/cube.stl", &cube_stl(), None, false).unwrap();
        let s = crate::decode_named_bytes_for_export("parts/cube.stl", &cube_stl()).unwrap();
        assert_eq!((s.width, s.height), (shown.width, shown.height));
        assert_eq!(s.channels, pb_encode::Channels::Rgba);
        let pb_encode::Samples::U8(px) = &s.samples else {
            panic!("a model renders 8-bit");
        };
        assert_eq!(px[3], 0, "the corner is transparent");
        assert!(crate::decode_named_bytes_for_export("x.stl", b"nope").is_err());
    }

    #[test]
    fn garbage_model_is_a_decode_error_not_a_panic() {
        assert!(decode_named_bytes("x.stl", b"nope", None, false).is_err());
        assert!(decode_named_bytes("x.3mf", b"nope", None, false).is_err());
        assert!(decode_named_bytes("x.obj", b"\xff\xfe\x00", None, false).is_err());
    }

    #[test]
    fn a_3mf_without_a_mesh_falls_back_to_its_thumbnail() {
        use image::ImageEncoder;
        use std::io::Write;
        // A 1x1 PNG as the declared thumbnail, and a model part with an empty build.
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[10, 20, 30, 255], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Target="/3D/3dmodel.model" Id="r0" Type="http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel"/><Relationship Target="/Metadata/plate_1.png" Id="r1" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/thumbnail"/></Relationships>"#;
        let model = r#"<model unit="millimeter" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02"><resources/><build/></model>"#;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let o = zip::write::SimpleFileOptions::default();
            for (n, d) in [
                ("_rels/.rels", rels.as_bytes()),
                ("3D/3dmodel.model", model.as_bytes()),
                ("Metadata/plate_1.png", png.as_slice()),
            ] {
                z.start_file(n, o).unwrap();
                z.write_all(d).unwrap();
            }
            z.finish().unwrap();
        }
        let img = decode_named_bytes("plate.gcode.3mf", &buf.into_inner(), None, false).unwrap();
        assert_eq!(img.codec, "3MF");
        assert_eq!((img.width, img.height), (1, 1));
    }
}
