//! **3D models** (task #151; plan: `.taskmaster/docs/3d-models-plan.md`).
//!
//! Bytes in, one canonical [`Model`] out, then a still picture of it ([`raster`]). Pure: no
//! `std::fs`, no GPU, no platform code, so every parser is unit-testable and fuzzable, and the
//! live 3D view (task #153) can reuse the same model and camera on the GPU.
//!
//! Every format is lowered to **Z-up**, the axis 3MF mandates and the 3D-printing convention
//! for STL. Y-up formats (OBJ) are rotated at parse time, so the camera never has to ask.
//!
//! Parsers treat their input as hostile: sizes are checked before anything is allocated
//! ([`Limits`]), indices are validated, non-finite vertices are dropped, and a 3MF's component
//! tree is depth-limited and cycle-checked. `pb-decode` additionally runs every decode inside
//! `catch_unwind`.

mod camera;
pub mod live;
mod obj;
pub mod raster;
mod stl;
mod threemf;

pub use live::{LiveFrame, LiveView};
pub use raster::{CpuRasterizer, MeshRasterizer, RenderOptions, Rendered};

/// The default material colour (owner decision D3, 2026-11-23): a neutral light grey, used
/// for every triangle a file doesn't colour itself. sRGB.
pub const DEFAULT_COLOR: [u8; 3] = [200, 204, 210];

/// A 3D file format we parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFormat {
    Stl,
    Obj,
    ThreeMf,
}

impl ModelFormat {
    /// Classify a file extension (no dot, any case).
    pub fn from_extension(ext: &str) -> Option<ModelFormat> {
        if ext.eq_ignore_ascii_case("stl") {
            Some(ModelFormat::Stl)
        } else if ext.eq_ignore_ascii_case("obj") {
            Some(ModelFormat::Obj)
        } else if ext.eq_ignore_ascii_case("3mf") {
            Some(ModelFormat::ThreeMf)
        } else {
            None
        }
    }

    /// The display name the Info panel shows as the codec.
    pub fn codec_name(self) -> &'static str {
        match self {
            ModelFormat::Stl => "STL",
            ModelFormat::Obj => "OBJ",
            ModelFormat::ThreeMf => "3MF",
        }
    }
}

/// The length unit a file declares. STL and OBJ declare none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Units {
    Micron,
    Millimetre,
    Centimetre,
    Inch,
    Foot,
    Metre,
    #[default]
    Unknown,
}

impl Units {
    /// Millimetres per unit, or `None` when the file didn't say.
    pub fn to_mm(self) -> Option<f64> {
        match self {
            Units::Micron => Some(0.001),
            Units::Millimetre => Some(1.0),
            Units::Centimetre => Some(10.0),
            Units::Inch => Some(25.4),
            Units::Foot => Some(304.8),
            Units::Metre => Some(1000.0),
            Units::Unknown => None,
        }
    }
}

/// What the Info panel can say about a model beyond its picture.
#[derive(Debug, Clone, Default)]
pub struct ModelFacts {
    pub units: Units,
    /// Top-level objects placed in the scene (a 3MF's build items; 1 for STL/OBJ).
    pub objects: u32,
    /// Triangles dropped as malformed (out-of-range index, non-finite vertex, degenerate data).
    pub skipped_triangles: u64,
    /// A 3MF's `<metadata>` (Title, Designer, Application, …), capped in count and length.
    pub metadata: Vec<(String, String)>,
    /// A 3MF's embedded thumbnail (PNG or JPEG bytes), from the package's OPC thumbnail
    /// relationship. The fallback picture when there's no mesh to draw (owner decision D2).
    pub thumbnail: Option<Vec<u8>>,
}

/// An axis-aligned bounding box.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

impl Bounds {
    pub fn center(&self) -> [f32; 3] {
        [
            (self.min[0] + self.max[0]) * 0.5,
            (self.min[1] + self.max[1]) * 0.5,
            (self.min[2] + self.max[2]) * 0.5,
        ]
    }

    pub fn size(&self) -> [f32; 3] {
        [
            self.max[0] - self.min[0],
            self.max[1] - self.min[1],
            self.max[2] - self.min[2],
        ]
    }
}

/// The canonical mesh every format lowers to: an indexed triangle list in Z-up world space,
/// with every build transform already applied. Flat `f32` positions and `u32` indices, so the
/// live view (#153) can upload it as-is.
#[derive(Debug, Clone)]
pub struct Model {
    pub format: ModelFormat,
    pub positions: Vec<[f32; 3]>,
    pub triangles: Vec<[u32; 3]>,
    /// Per-triangle sRGB colour, parallel to `triangles`, when the file colours anything.
    /// Triangles it leaves uncoloured carry [`DEFAULT_COLOR`].
    pub triangle_colors: Option<Vec<[u8; 3]>>,
    pub facts: ModelFacts,
}

impl Model {
    fn new(format: ModelFormat) -> Model {
        Model {
            format,
            positions: Vec::new(),
            triangles: Vec::new(),
            triangle_colors: None,
            facts: ModelFacts::default(),
        }
    }

    /// Nothing to draw.
    pub fn is_empty(&self) -> bool {
        self.triangles.is_empty()
    }

    /// The bounding box of every vertex a triangle uses (unreferenced vertices don't count),
    /// or `None` for an empty model.
    pub fn bounds(&self) -> Option<Bounds> {
        let mut min = [f32::INFINITY; 3];
        let mut max = [f32::NEG_INFINITY; 3];
        for tri in &self.triangles {
            for &i in tri {
                let p = self.positions[i as usize];
                for a in 0..3 {
                    min[a] = min[a].min(p[a]);
                    max[a] = max[a].max(p[a]);
                }
            }
        }
        (!self.triangles.is_empty()).then_some(Bounds { min, max })
    }

    /// The model's width × depth × height in millimetres, when its units are known.
    pub fn size_mm(&self) -> Option<[f64; 3]> {
        let mm = self.facts.units.to_mm()?;
        let s = self.bounds()?.size();
        Some([s[0] as f64 * mm, s[1] as f64 * mm, s[2] as f64 * mm])
    }

    /// Drop every triangle that references a missing or non-finite vertex, keeping
    /// `triangle_colors` aligned, and count what was dropped. Every parser ends here, so
    /// the renderer and the bounds can trust what they get.
    fn sanitize(&mut self) {
        let finite: Vec<bool> = self
            .positions
            .iter()
            .map(|p| p.iter().all(|c| c.is_finite()))
            .collect();
        let ok = |t: &[u32; 3]| {
            t.iter()
                .all(|&i| finite.get(i as usize).copied().unwrap_or(false))
        };
        let before = self.triangles.len();
        match self.triangle_colors.as_mut() {
            Some(colors) => {
                let mut w = 0;
                for r in 0..self.triangles.len() {
                    if ok(&self.triangles[r]) {
                        self.triangles[w] = self.triangles[r];
                        colors[w] = colors[r];
                        w += 1;
                    }
                }
                self.triangles.truncate(w);
                colors.truncate(w);
            }
            None => self.triangles.retain(|t| ok(t)),
        }
        self.facts.skipped_triangles += (before - self.triangles.len()) as u64;
    }
}

/// Refuse-before-reserve caps, checked against cheap header facts before anything big is
/// allocated. Fixed for now; deriving them from the machine's RAM is #151 follow-up work.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Most triangles a model may expand to (instancing and 3MF components included).
    /// Thingi10K's largest model is 5.67M; its median is 4.6k.
    pub max_triangles: usize,
    /// Largest single input (a whole STL/OBJ, or one inflated part of a 3MF package).
    pub max_part_bytes: u64,
    /// Most model parts a 3MF may pull in through Production-extension references.
    pub max_parts: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_triangles: 10_000_000,
            max_part_bytes: 1 << 30,
            max_parts: 4096,
        }
    }
}

/// Why a model couldn't be parsed or drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MeshError {
    /// Not a valid file of the claimed format.
    Malformed(String),
    /// Bigger than [`Limits`] allow; refused before allocating.
    TooLarge(String),
    /// Parsed, but there is nothing to draw.
    Empty,
}

impl std::fmt::Display for MeshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MeshError::Malformed(why) => write!(f, "malformed model: {why}"),
            MeshError::TooLarge(why) => write!(f, "model too large: {why}"),
            MeshError::Empty => write!(f, "model has no triangles"),
        }
    }
}

impl std::error::Error for MeshError {}

/// Parse `bytes` as `format` into a [`Model`]. An empty result is `Ok` (a 3MF may carry only
/// a thumbnail — see [`ModelFacts::thumbnail`]); callers check [`Model::is_empty`].
pub fn parse(format: ModelFormat, bytes: &[u8], limits: &Limits) -> Result<Model, MeshError> {
    let mut model = match format {
        ModelFormat::Stl => stl::parse(bytes, limits)?,
        ModelFormat::Obj => obj::parse(bytes, limits)?,
        ModelFormat::ThreeMf => threemf::parse(bytes, limits)?,
    };
    model.sanitize();
    Ok(model)
}

/// A 3MF's embedded thumbnail without parsing its meshes: the picture to show when the mesh
/// can't be drawn (too large, malformed, or a sliced `.gcode.3mf` with no mesh).
pub fn threemf_thumbnail(bytes: &[u8]) -> Option<Vec<u8>> {
    threemf::thumbnail(bytes)
}

#[cfg(test)]
pub(crate) mod test_util {
    /// A unit cube as binary STL (12 triangles, outward winding).
    pub fn cube_stl_binary() -> Vec<u8> {
        let tris = cube_triangles();
        let mut out = vec![0u8; 80];
        out.extend_from_slice(&(tris.len() as u32).to_le_bytes());
        for t in &tris {
            out.extend_from_slice(&[0u8; 12]); // normal (ignored; recomputed)
            for v in t {
                for c in v {
                    out.extend_from_slice(&c.to_le_bytes());
                }
            }
            out.extend_from_slice(&[0, 0]);
        }
        out
    }

    pub fn cube_triangles() -> Vec<[[f32; 3]; 3]> {
        let v = |x: f32, y: f32, z: f32| [x, y, z];
        let quads = [
            // bottom (z=0), top (z=1), front (y=0), back (y=1), left (x=0), right (x=1)
            [v(0., 0., 0.), v(0., 1., 0.), v(1., 1., 0.), v(1., 0., 0.)],
            [v(0., 0., 1.), v(1., 0., 1.), v(1., 1., 1.), v(0., 1., 1.)],
            [v(0., 0., 0.), v(1., 0., 0.), v(1., 0., 1.), v(0., 0., 1.)],
            [v(0., 1., 0.), v(0., 1., 1.), v(1., 1., 1.), v(1., 1., 0.)],
            [v(0., 0., 0.), v(0., 0., 1.), v(0., 1., 1.), v(0., 1., 0.)],
            [v(1., 0., 0.), v(1., 1., 0.), v(1., 1., 1.), v(1., 0., 1.)],
        ];
        quads
            .iter()
            .flat_map(|q| [[q[0], q[1], q[2]], [q[0], q[2], q[3]]])
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_classify_case_insensitively() {
        assert_eq!(ModelFormat::from_extension("STL"), Some(ModelFormat::Stl));
        assert_eq!(ModelFormat::from_extension("obj"), Some(ModelFormat::Obj));
        assert_eq!(
            ModelFormat::from_extension("3Mf"),
            Some(ModelFormat::ThreeMf)
        );
        assert_eq!(ModelFormat::from_extension("zip"), None);
        assert_eq!(ModelFormat::from_extension(""), None);
    }

    #[test]
    fn sanitize_drops_bad_triangles_and_keeps_colours_aligned() {
        let mut m = Model::new(ModelFormat::Stl);
        m.positions = vec![
            [0.0; 3],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [f32::NAN, 0.0, 0.0],
        ];
        m.triangles = vec![[0, 1, 2], [0, 1, 3], [0, 1, 9], [2, 1, 0]];
        m.triangle_colors = Some(vec![[1, 1, 1], [2, 2, 2], [3, 3, 3], [4, 4, 4]]);
        m.sanitize();
        assert_eq!(m.triangles, vec![[0, 1, 2], [2, 1, 0]]);
        assert_eq!(m.triangle_colors, Some(vec![[1, 1, 1], [4, 4, 4]]));
        assert_eq!(m.facts.skipped_triangles, 2);
    }

    #[test]
    fn bounds_ignore_unreferenced_vertices() {
        let mut m = Model::new(ModelFormat::Stl);
        m.positions = vec![
            [0.0; 3],
            [2.0, 0.0, 0.0],
            [0.0, 3.0, 1.0],
            [100.0, 100.0, 100.0],
        ];
        m.triangles = vec![[0, 1, 2]];
        let b = m.bounds().unwrap();
        assert_eq!(b.min, [0.0, 0.0, 0.0]);
        assert_eq!(b.max, [2.0, 3.0, 1.0]);
        assert!(Model::new(ModelFormat::Obj).bounds().is_none());
    }

    #[test]
    fn size_mm_needs_known_units() {
        let mut m = parse(
            ModelFormat::Stl,
            &test_util::cube_stl_binary(),
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(m.size_mm(), None, "STL is unitless");
        m.facts.units = Units::Centimetre;
        assert_eq!(m.size_mm(), Some([10.0, 10.0, 10.0]));
    }

    proptest::proptest! {
        /// No parser may panic on arbitrary bytes (the fuzz targets go further).
        #[test]
        fn parsers_never_panic(bytes in proptest::collection::vec(proptest::num::u8::ANY, 0..2048)) {
            for f in [ModelFormat::Stl, ModelFormat::Obj, ModelFormat::ThreeMf] {
                let _ = parse(f, &bytes, &Limits::default());
            }
            let _ = threemf_thumbnail(&bytes);
        }
    }
}
