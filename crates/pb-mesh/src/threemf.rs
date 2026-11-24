//! 3MF, hand-rolled over `zip` + `quick-xml` (plan § Libraries: no crate offered budgets,
//! cancellation-friendly streaming, and a merged Production-extension scene together).
//!
//! A 3MF is an OPC package (a ZIP). `_rels/.rels` names the root model part (usually
//! `3D/3dmodel.model`) and, for files from every mainstream slicer, a thumbnail. A model part
//! holds `<resources>` (objects, which are either a `<mesh>` or `<components>` placing other
//! objects, plus colour resources) and a `<build>` of items placing objects in the scene.
//!
//! Supported: the core spec (meshes, components, build transforms, units, metadata), the
//! **Production extension**'s cross-part references (`p:path` on components and items, which
//! is how Bambu Studio and OrcaSlicer store every object, so without it their projects render
//! empty), and colour from `<basematerials>` / `<m:colorgroup>`. Not yet: textures, beam
//! lattices, slices, and the slicer-specific extras (#152).
//!
//! Elements are matched by **local name**, ignoring the namespace prefix: files bind the
//! extension namespaces to whatever prefix they like, and nothing the core and the
//! extensions we read define collides on local name.
//!
//! Hostile input: every part's inflated size is capped before reading (ZIP bombs), the output
//! triangle count is capped (instancing can multiply a small file), the component tree is
//! depth-limited and cycle-checked, and a missing object or bad index drops geometry rather
//! than failing the file.

use std::collections::HashMap;
use std::io::{Cursor, Read};

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use zip::ZipArchive;

use crate::{Limits, MeshError, Model, ModelFormat, Units, DEFAULT_COLOR};

const REL_START_PART: &str = "http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel";
const REL_THUMBNAIL: &str =
    "http://schemas.openxmlformats.org/package/2006/relationships/metadata/thumbnail";
const DEFAULT_ROOT: &str = "3D/3dmodel.model";
const MAX_RELS_BYTES: u64 = 1 << 20;
const MAX_THUMBNAIL_BYTES: u64 = 32 << 20;
const MAX_DEPTH: usize = 32;
const MAX_METADATA: usize = 64;
const MAX_METADATA_LEN: usize = 1024;

pub(crate) fn parse(bytes: &[u8], limits: &Limits) -> Result<Model, MeshError> {
    let mut pkg = Package::open(bytes)?;
    let rels = pkg.rels();
    let root_name = rels
        .start
        .clone()
        .unwrap_or_else(|| DEFAULT_ROOT.to_string());

    // Load the root part, then every part it (transitively) references, before assembling,
    // so the assembly pass can borrow all of them at once.
    let mut parts: HashMap<String, Part> = HashMap::new();
    let mut pending = vec![root_name.clone()];
    while let Some(name) = pending.pop() {
        if parts.contains_key(&name) {
            continue;
        }
        if parts.len() >= limits.max_parts {
            return Err(MeshError::TooLarge(format!(
                "more than {} model parts",
                limits.max_parts
            )));
        }
        let xml = match pkg.read(&name, limits.max_part_bytes) {
            Ok(xml) => xml,
            // A missing root is fatal; a missing referenced part just loses its objects.
            Err(e) if name == root_name => return Err(e),
            Err(MeshError::TooLarge(why)) => return Err(MeshError::TooLarge(why)),
            Err(_) => continue,
        };
        let part = parse_part(&xml, limits)?;
        pending.extend(
            part.referenced_paths()
                .filter(|p| !parts.contains_key(*p))
                .map(str::to_string),
        );
        parts.insert(name, part);
    }
    let root = parts
        .get(&root_name)
        .ok_or_else(|| MeshError::Malformed("no root model part".into()))?;

    let mut out = Assembly {
        parts: &parts,
        limits,
        model: Model::new(ModelFormat::ThreeMf),
        colors: Vec::new(),
        any_color: false,
        stack: Vec::new(),
    };
    for item in &root.build {
        let path = item.path.as_deref().unwrap_or(&root_name);
        out.place(path, item.objectid, &item.transform, 0)?;
    }
    let mut model = out.model;
    if out.any_color {
        model.triangle_colors = Some(out.colors);
    }
    model.facts.units = root.unit;
    model.facts.objects = root.build.len() as u32;
    model.facts.metadata = root.metadata.clone();
    model.facts.thumbnail = rels
        .thumbnail
        .and_then(|t| pkg.read(&t, MAX_THUMBNAIL_BYTES).ok());
    Ok(model)
}

/// The package's thumbnail, without parsing any mesh. Follows the OPC thumbnail relationship
/// (every mainstream slicer declares one), falling back to the conventional
/// `Metadata/thumbnail.png` and Bambu's `Metadata/plate_1.png`. Never Bambu's `pick_N.png`,
/// which is an object-ID render, not a picture.
pub(crate) fn thumbnail(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut pkg = Package::open(bytes).ok()?;
    let rels = pkg.rels();
    rels.thumbnail
        .into_iter()
        .chain([
            "Metadata/thumbnail.png".to_string(),
            "Metadata/plate_1.png".to_string(),
        ])
        .find_map(|name| pkg.read(&name, MAX_THUMBNAIL_BYTES).ok())
}

// ---------------------------------------------------------------------------------------------
// The package (ZIP) layer.
// ---------------------------------------------------------------------------------------------

struct Package<'a> {
    zip: ZipArchive<Cursor<&'a [u8]>>,
}

#[derive(Default)]
struct Rels {
    start: Option<String>,
    thumbnail: Option<String>,
}

impl<'a> Package<'a> {
    fn open(bytes: &'a [u8]) -> Result<Package<'a>, MeshError> {
        let zip = ZipArchive::new(Cursor::new(bytes))
            .map_err(|e| MeshError::Malformed(format!("not a 3MF package: {e}")))?;
        Ok(Package { zip })
    }

    /// Read part `name` (an OPC part name, leading `/` optional), refusing anything whose
    /// inflated size exceeds `cap`: checked against the declared size first, then enforced on
    /// the actual stream in case the declaration lies.
    fn read(&mut self, name: &str, cap: u64) -> Result<Vec<u8>, MeshError> {
        let wanted = name.trim_start_matches('/');
        // Part names are case-insensitive in OPC; ZIP lookups are not.
        let entry = if self.zip.index_for_name(wanted).is_some() {
            wanted.to_string()
        } else {
            self.zip
                .file_names()
                .filter_map(Result::ok)
                .find(|n| n.eq_ignore_ascii_case(wanted))
                .map(|n| n.into_owned())
                .ok_or_else(|| MeshError::Malformed(format!("missing part {wanted}")))?
        };
        let file = self
            .zip
            .by_name(&entry)
            .map_err(|e| MeshError::Malformed(format!("part {wanted}: {e}")))?;
        if file.size() > cap {
            return Err(MeshError::TooLarge(format!(
                "part {wanted} inflates to {} bytes (limit {cap})",
                file.size()
            )));
        }
        let mut out = Vec::with_capacity(file.size() as usize);
        file.take(cap + 1)
            .read_to_end(&mut out)
            .map_err(|e| MeshError::Malformed(format!("part {wanted}: {e}")))?;
        if out.len() as u64 > cap {
            return Err(MeshError::TooLarge(format!(
                "part {wanted} exceeds {cap} bytes"
            )));
        }
        Ok(out)
    }

    fn rels(&mut self) -> Rels {
        let Ok(xml) = self.read("_rels/.rels", MAX_RELS_BYTES) else {
            return Rels::default();
        };
        let mut rels = Rels::default();
        let mut reader = Reader::from_reader(xml.as_slice());
        loop {
            match reader.read_event() {
                Ok(Event::Start(e) | Event::Empty(e))
                    if e.local_name().as_ref() == "Relationship" =>
                {
                    let (mut target, mut kind) = (None, None);
                    for a in e.attributes().flatten() {
                        match a.key.local_name().as_ref() {
                            "Target" => target = Some(a.value.to_string()),
                            "Type" => kind = Some(a.value.to_string()),
                            _ => {}
                        }
                    }
                    if let (Some(target), Some(kind)) = (target, kind) {
                        if kind == REL_START_PART && rels.start.is_none() {
                            rels.start = Some(target.trim_start_matches('/').to_string());
                        } else if kind == REL_THUMBNAIL && rels.thumbnail.is_none() {
                            rels.thumbnail = Some(target.trim_start_matches('/').to_string());
                        }
                    }
                }
                Ok(Event::Eof) | Err(_) => break,
                _ => {}
            }
        }
        rels
    }
}

// ---------------------------------------------------------------------------------------------
// One model part.
// ---------------------------------------------------------------------------------------------

/// An affine transform in 3MF's row-vector convention: `p' = p · M`, stored as the spec
/// writes it — `m00 m01 m02 m10 m11 m12 m20 m21 m22 m30 m31 m32`, the last row translation.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Transform([f32; 12]);

impl Transform {
    const IDENTITY: Transform = Transform([1., 0., 0., 0., 1., 0., 0., 0., 1., 0., 0., 0.]);

    fn parse(s: &str) -> Option<Transform> {
        let mut m = [0f32; 12];
        let mut it = s.split_ascii_whitespace();
        for v in &mut m {
            *v = it.next()?.parse().ok()?;
        }
        (it.next().is_none() && m.iter().all(|v| v.is_finite())).then_some(Transform(m))
    }

    fn apply(&self, p: [f32; 3]) -> [f32; 3] {
        let m = &self.0;
        [
            p[0] * m[0] + p[1] * m[3] + p[2] * m[6] + m[9],
            p[0] * m[1] + p[1] * m[4] + p[2] * m[7] + m[10],
            p[0] * m[2] + p[1] * m[5] + p[2] * m[8] + m[11],
        ]
    }

    /// `self` applied first, then `outer`.
    fn then(&self, outer: &Transform) -> Transform {
        let (a, b) = (&self.0, &outer.0);
        let mut m = [0f32; 12];
        for r in 0..3 {
            for c in 0..3 {
                m[r * 3 + c] = (0..3).map(|k| a[r * 3 + k] * b[k * 3 + c]).sum();
            }
        }
        for c in 0..3 {
            m[9 + c] = (0..3).map(|k| a[9 + k] * b[k * 3 + c]).sum::<f32>() + b[9 + c];
        }
        Transform(m)
    }
}

/// "No property" in a triangle's (pid, p1) pair.
const NONE: u32 = u32::MAX;

#[derive(Default)]
struct Mesh {
    vertices: Vec<[f32; 3]>,
    triangles: Vec<[u32; 3]>,
    /// Per-triangle `(pid, p1)`, parallel to `triangles` — allocated only once some triangle
    /// carries a property, so uncoloured meshes pay nothing.
    props: Vec<(u32, u32)>,
}

struct Component {
    objectid: u32,
    transform: Transform,
    path: Option<String>,
}

#[derive(Default)]
struct Object {
    mesh: Option<Mesh>,
    components: Vec<Component>,
    pid: Option<u32>,
    pindex: Option<u32>,
}

struct Item {
    objectid: u32,
    transform: Transform,
    path: Option<String>,
}

#[derive(Default)]
struct Part {
    unit: Units,
    objects: HashMap<u32, Object>,
    /// `<basematerials>` and `<m:colorgroup>` resources alike: id → sRGB colours by index.
    colors: HashMap<u32, Vec<[u8; 3]>>,
    build: Vec<Item>,
    metadata: Vec<(String, String)>,
}

impl Part {
    fn referenced_paths(&self) -> impl Iterator<Item = &str> {
        self.objects
            .values()
            .flat_map(|o| o.components.iter().filter_map(|c| c.path.as_deref()))
            .chain(self.build.iter().filter_map(|i| i.path.as_deref()))
    }
}

fn attr_u32(e: &BytesStart, name: &str) -> Option<u32> {
    attr(e, name)?.trim().parse().ok()
}

fn attr(e: &BytesStart, name: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == name)
        .map(|a| a.value.to_string())
}

/// A part reference as written (`/3D/Objects/object_1.model`) → its ZIP entry name.
fn part_path(e: &BytesStart) -> Option<String> {
    attr(e, "path").map(|p| p.trim_start_matches('/').to_string())
}

/// `#RRGGBB` or `#RRGGBBAA` (alpha ignored: the still renders everything opaque).
fn parse_color(s: &str) -> Option<[u8; 3]> {
    let hex = s.trim().strip_prefix('#')?;
    if hex.len() != 6 && hex.len() != 8 {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
    Some([byte(0)?, byte(2)?, byte(4)?])
}

fn parse_units(s: &str) -> Units {
    match s {
        "micron" => Units::Micron,
        "millimeter" => Units::Millimetre,
        "centimeter" => Units::Centimetre,
        "inch" => Units::Inch,
        "foot" => Units::Foot,
        "meter" => Units::Metre,
        _ => Units::Unknown,
    }
}

fn parse_part(xml: &[u8], limits: &Limits) -> Result<Part, MeshError> {
    let mut part = Part {
        // The spec's default unit.
        unit: Units::Millimetre,
        ..Part::default()
    };
    let mut reader = Reader::from_reader(xml);
    let mut object: Option<(u32, Object)> = None;
    let mut color_group: Option<(u32, Vec<[u8; 3]>)> = None;
    let mut metadata: Option<(String, String)> = None;
    let mut triangles_seen = 0usize;

    loop {
        let event = reader
            .read_event()
            .map_err(|e| MeshError::Malformed(format!("model XML: {e}")))?;
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(event, Event::Empty(_));
                match e.local_name().as_ref() {
                    "model" => {
                        part.unit = attr(e, "unit").map_or(Units::Millimetre, |u| parse_units(&u));
                    }
                    "metadata" if object.is_none() && !empty => {
                        if let Some(name) = attr(e, "name") {
                            metadata = Some((name, String::new()));
                        }
                    }
                    "object" => {
                        let Some(id) = attr_u32(e, "id") else {
                            continue;
                        };
                        let obj = Object {
                            pid: attr_u32(e, "pid"),
                            pindex: attr_u32(e, "pindex"),
                            ..Object::default()
                        };
                        if empty {
                            part.objects.insert(id, obj);
                        } else {
                            object = Some((id, obj));
                        }
                    }
                    "mesh" => {
                        if let Some((_, o)) = object.as_mut() {
                            o.mesh = Some(Mesh::default());
                        }
                    }
                    "vertex" => {
                        if let Some(mesh) = object.as_mut().and_then(|(_, o)| o.mesh.as_mut()) {
                            let c = |n| attr(e, n).and_then(|v| v.trim().parse::<f32>().ok());
                            // A missing coordinate becomes NaN, so `sanitize` drops what uses it.
                            mesh.vertices.push([
                                c("x").unwrap_or(f32::NAN),
                                c("y").unwrap_or(f32::NAN),
                                c("z").unwrap_or(f32::NAN),
                            ]);
                        }
                    }
                    "triangle" => {
                        if let Some(mesh) = object.as_mut().and_then(|(_, o)| o.mesh.as_mut()) {
                            triangles_seen += 1;
                            if triangles_seen > limits.max_triangles {
                                return Err(MeshError::TooLarge(format!(
                                    "more than {} triangles",
                                    limits.max_triangles
                                )));
                            }
                            let v = |n| attr_u32(e, n).unwrap_or(NONE);
                            mesh.triangles.push([v("v1"), v("v2"), v("v3")]);
                            let (pid, p1) = (v("pid"), v("p1"));
                            if pid != NONE || p1 != NONE {
                                mesh.props.resize(mesh.triangles.len() - 1, (NONE, NONE));
                                mesh.props.push((pid, p1));
                            }
                        }
                    }
                    "component" => {
                        if let (Some((_, o)), Some(objectid)) =
                            (object.as_mut(), attr_u32(e, "objectid"))
                        {
                            o.components.push(Component {
                                objectid,
                                transform: attr(e, "transform")
                                    .and_then(|t| Transform::parse(&t))
                                    .unwrap_or(Transform::IDENTITY),
                                path: part_path(e),
                            });
                        }
                    }
                    "basematerials" | "colorgroup" if !empty => {
                        if let Some(id) = attr_u32(e, "id") {
                            color_group = Some((id, Vec::new()));
                        }
                    }
                    "base" | "color" => {
                        if let Some((_, colors)) = color_group.as_mut() {
                            let key = if e.local_name().as_ref() == "base" {
                                "displaycolor"
                            } else {
                                "color"
                            };
                            // Keep indices aligned even when a colour is unreadable.
                            colors.push(
                                attr(e, key)
                                    .and_then(|c| parse_color(&c))
                                    .unwrap_or(DEFAULT_COLOR),
                            );
                        }
                    }
                    "item" => {
                        if let Some(objectid) = attr_u32(e, "objectid") {
                            part.build.push(Item {
                                objectid,
                                transform: attr(e, "transform")
                                    .and_then(|t| Transform::parse(&t))
                                    .unwrap_or(Transform::IDENTITY),
                                path: part_path(e),
                            });
                        }
                    }
                    _ => {}
                }
            }
            Event::End(ref e) => match e.local_name().as_ref() {
                "object" => {
                    if let Some((id, o)) = object.take() {
                        part.objects.insert(id, o);
                    }
                }
                "basematerials" | "colorgroup" => {
                    if let Some((id, colors)) = color_group.take() {
                        part.colors.insert(id, colors);
                    }
                }
                "metadata" => {
                    if let Some((name, value)) = metadata.take() {
                        if part.metadata.len() < MAX_METADATA && !value.trim().is_empty() {
                            part.metadata.push((name, value.trim().to_string()));
                        }
                    }
                }
                _ => {}
            },
            Event::Text(ref t) => {
                if let Some((_, value)) = metadata.as_mut() {
                    push_capped(value, &t.xml10_content());
                }
            }
            Event::GeneralRef(ref r) => {
                if let Some((_, value)) = metadata.as_mut() {
                    let s = match r.xml10_content().as_ref() {
                        "amp" => "&",
                        "lt" => "<",
                        "gt" => ">",
                        "quot" => "\"",
                        "apos" => "'",
                        _ => "",
                    };
                    push_capped(value, s);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(part)
}

fn push_capped(value: &mut String, s: &str) {
    for ch in s.chars() {
        if value.len() + ch.len_utf8() > MAX_METADATA_LEN {
            return;
        }
        value.push(ch);
    }
}

// ---------------------------------------------------------------------------------------------
// Assembly: flatten the build into one world-space mesh.
// ---------------------------------------------------------------------------------------------

struct Assembly<'p> {
    parts: &'p HashMap<String, Part>,
    limits: &'p Limits,
    model: Model,
    colors: Vec<[u8; 3]>,
    any_color: bool,
    /// The (part, object) chain being placed, for cycle detection.
    stack: Vec<(&'p str, u32)>,
}

impl<'p> Assembly<'p> {
    fn place(
        &mut self,
        path: &'p str,
        id: u32,
        transform: &Transform,
        depth: usize,
    ) -> Result<(), MeshError> {
        if depth > MAX_DEPTH {
            return Err(MeshError::Malformed("components nested too deeply".into()));
        }
        if self.stack.contains(&(path, id)) {
            return Err(MeshError::Malformed("component cycle".into()));
        }
        let parts = self.parts;
        let Some(part) = parts.get(path) else {
            return Ok(()); // a referenced part we couldn't read: its objects are lost
        };
        let Some(object) = part.objects.get(&id) else {
            return Ok(()); // a dangling reference: skip it, keep the rest of the scene
        };
        self.stack.push((path, id));
        if let Some(mesh) = &object.mesh {
            self.append_mesh(part, object, mesh, transform)?;
        }
        for c in &object.components {
            let child_path = c.path.as_deref().unwrap_or(path);
            self.place(
                child_path,
                c.objectid,
                &c.transform.then(transform),
                depth + 1,
            )?;
        }
        self.stack.pop();
        Ok(())
    }

    fn append_mesh(
        &mut self,
        part: &Part,
        object: &Object,
        mesh: &Mesh,
        t: &Transform,
    ) -> Result<(), MeshError> {
        if self.model.triangles.len() + mesh.triangles.len() > self.limits.max_triangles {
            return Err(MeshError::TooLarge(format!(
                "more than {} triangles once placed",
                self.limits.max_triangles
            )));
        }
        let base = self.model.positions.len();
        let n = mesh.vertices.len();
        self.model
            .positions
            .extend(mesh.vertices.iter().map(|&p| t.apply(p)));
        for (i, tri) in mesh.triangles.iter().enumerate() {
            let idx = |v: u32| {
                if (v as usize) < n {
                    (base + v as usize) as u32
                } else {
                    u32::MAX // dropped and counted by `sanitize`
                }
            };
            self.model
                .triangles
                .push([idx(tri[0]), idx(tri[1]), idx(tri[2])]);
            let (pid, p1) = mesh.props.get(i).copied().unwrap_or((NONE, NONE));
            let pid = if pid != NONE { Some(pid) } else { object.pid };
            let index = if p1 != NONE { Some(p1) } else { object.pindex };
            let color = pid
                .zip(index)
                .and_then(|(pid, index)| part.colors.get(&pid)?.get(index as usize).copied());
            self.any_color |= color.is_some();
            self.colors.push(color.unwrap_or(DEFAULT_COLOR));
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    const RELS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
 <Relationship Target="/3D/3dmodel.model" Id="rel0" Type="http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel"/>
 <Relationship Target="/Metadata/thumbnail.png" Id="rel1" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/thumbnail"/>
</Relationships>"#;

    /// A tetrahedron object `id` (4 vertices, 4 triangles).
    fn tetra(id: u32, extra: &str) -> String {
        format!(
            r#"<object id="{id}" type="model" {extra}><mesh><vertices>
<vertex x="0" y="0" z="0"/><vertex x="10" y="0" z="0"/><vertex x="0" y="10" z="0"/><vertex x="0" y="0" z="10"/>
</vertices><triangles>
<triangle v1="0" v2="2" v3="1"/><triangle v1="0" v2="1" v3="3"/><triangle v1="0" v2="3" v3="2"/><triangle v1="1" v2="2" v3="3"/>
</triangles></mesh></object>"#
        )
    }

    pub(crate) fn package(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for (name, data) in files {
                z.start_file(*name, opts).unwrap();
                z.write_all(data).unwrap();
            }
            z.finish().unwrap();
        }
        buf.into_inner()
    }

    fn model_xml(resources: &str, build: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<model unit="millimeter" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02"
 xmlns:p="http://schemas.microsoft.com/3dmanufacturing/production/2015/06"
 xmlns:m="http://schemas.microsoft.com/3dmanufacturing/material/2015/02">
<metadata name="Title">Test &amp; tetra</metadata>
<metadata name="Application">pb-mesh tests</metadata>
<resources>{resources}</resources><build>{build}</build></model>"#
        )
    }

    pub(crate) fn simple_3mf() -> Vec<u8> {
        let xml = model_xml(&tetra(1, ""), r#"<item objectid="1"/>"#);
        package(&[
            ("_rels/.rels", RELS.as_bytes()),
            ("3D/3dmodel.model", xml.as_bytes()),
        ])
    }

    fn parse_default(bytes: &[u8]) -> Result<Model, MeshError> {
        crate::parse(ModelFormat::ThreeMf, bytes, &Limits::default())
    }

    #[test]
    fn core_mesh_units_and_metadata() {
        let m = parse_default(&simple_3mf()).unwrap();
        assert_eq!(m.triangles.len(), 4);
        assert_eq!(m.facts.units, Units::Millimetre);
        assert_eq!(m.facts.objects, 1);
        assert_eq!(m.size_mm(), Some([10.0, 10.0, 10.0]));
        assert!(m.triangle_colors.is_none(), "nothing coloured");
        assert!(m
            .facts
            .metadata
            .contains(&("Title".to_string(), "Test & tetra".to_string())));
        assert!(
            m.facts.thumbnail.is_none(),
            "rels names one but the part is absent"
        );
    }

    #[test]
    fn build_transforms_and_instancing() {
        let xml = model_xml(
            &tetra(1, ""),
            r#"<item objectid="1"/><item objectid="1" transform="1 0 0 0 1 0 0 0 1 100 0 0"/>"#,
        );
        let m = parse_default(&package(&[("3D/3dmodel.model", xml.as_bytes())])).unwrap();
        assert_eq!(m.triangles.len(), 8, "placed twice");
        assert_eq!(m.bounds().unwrap().max[0], 110.0);
    }

    #[test]
    fn production_components_in_other_parts() {
        // The Bambu / Orca layout: the root holds only components pointing into other parts.
        let root = model_xml(
            r#"<object id="5" type="model"><components>
<component p:path="/3D/Objects/object_1.model" objectid="1" transform="1 0 0 0 1 0 0 0 1 0 0 5"/>
</components></object>"#,
            r#"<item objectid="5"/>"#,
        );
        let child = model_xml(&tetra(1, ""), "");
        let bytes = package(&[
            ("_rels/.rels", RELS.as_bytes()),
            ("3D/3dmodel.model", root.as_bytes()),
            ("3D/Objects/object_1.model", child.as_bytes()),
        ]);
        let m = parse_default(&bytes).unwrap();
        assert_eq!(m.triangles.len(), 4);
        let b = m.bounds().unwrap();
        assert_eq!(
            (b.min[2], b.max[2]),
            (5.0, 15.0),
            "component transform applied"
        );
    }

    #[test]
    fn base_materials_and_color_groups() {
        let resources = format!(
            r##"<basematerials id="2"><base name="Red" displaycolor="#FF0000"/><base name="Blue" displaycolor="#0000FFFF"/></basematerials>
<m:colorgroup id="3"><m:color color="#00FF00"/></m:colorgroup>{}"##,
            tetra(1, r#"pid="2" pindex="1""#).replace(
                r#"<triangle v1="0" v2="2" v3="1"/>"#,
                r#"<triangle v1="0" v2="2" v3="1" pid="3" p1="0"/>"#
            )
        );
        let xml = model_xml(&resources, r#"<item objectid="1"/>"#);
        let m = parse_default(&package(&[("3D/3dmodel.model", xml.as_bytes())])).unwrap();
        let c = m.triangle_colors.unwrap();
        assert_eq!(c[0], [0, 255, 0], "triangle-level colorgroup");
        assert_eq!(c[1], [0, 0, 255], "object default: basematerials index 1");
    }

    #[test]
    fn component_cycle_is_refused() {
        let xml = model_xml(
            r#"<object id="1"><components><component objectid="2"/></components></object>
<object id="2"><components><component objectid="1"/></components></object>"#,
            r#"<item objectid="1"/>"#,
        );
        let r = parse_default(&package(&[("3D/3dmodel.model", xml.as_bytes())]));
        assert!(matches!(r, Err(MeshError::Malformed(_))));
    }

    #[test]
    fn dangling_references_and_bad_indices_are_skipped() {
        let mut objects = tetra(1, "");
        objects = objects.replace(
            r#"<triangle v1="1" v2="2" v3="3"/>"#,
            r#"<triangle v1="1" v2="2" v3="99"/>"#,
        );
        let xml = model_xml(&objects, r#"<item objectid="1"/><item objectid="42"/>"#);
        let m = parse_default(&package(&[("3D/3dmodel.model", xml.as_bytes())])).unwrap();
        assert_eq!(m.triangles.len(), 3);
        assert_eq!(m.facts.skipped_triangles, 1);
    }

    #[test]
    fn instancing_cannot_exceed_the_triangle_cap() {
        let items: String = (0..10).map(|_| r#"<item objectid="1"/>"#).collect();
        let xml = model_xml(&tetra(1, ""), &items);
        let limits = Limits {
            max_triangles: 20,
            ..Limits::default()
        };
        let r = crate::parse(
            ModelFormat::ThreeMf,
            &package(&[("3D/3dmodel.model", xml.as_bytes())]),
            &limits,
        );
        assert!(matches!(r, Err(MeshError::TooLarge(_))));
    }

    #[test]
    fn oversized_part_is_refused_before_reading() {
        let limits = Limits {
            max_part_bytes: 64,
            ..Limits::default()
        };
        let r = crate::parse(ModelFormat::ThreeMf, &simple_3mf(), &limits);
        assert!(matches!(r, Err(MeshError::TooLarge(_))));
    }

    #[test]
    fn thumbnail_follows_the_relationship_and_skips_pick_images() {
        let xml = model_xml(&tetra(1, ""), r#"<item objectid="1"/>"#);
        let bytes = package(&[
            ("_rels/.rels", RELS.as_bytes()),
            ("3D/3dmodel.model", xml.as_bytes()),
            ("Metadata/pick_1.png", b"PICK"),
            ("Metadata/thumbnail.png", b"THUMB"),
        ]);
        assert_eq!(thumbnail(&bytes).as_deref(), Some(&b"THUMB"[..]));
        assert_eq!(
            parse_default(&bytes).unwrap().facts.thumbnail.as_deref(),
            Some(&b"THUMB"[..])
        );
        let only_pick = package(&[("Metadata/pick_1.png", b"PICK")]);
        assert_eq!(thumbnail(&only_pick), None);
    }

    /// A STORED ZIP built by hand the way Bambu Studio writes its 3MFs: every central-directory
    /// entry carries a ZIP64 extra field, and that field's local-header offset is **wrong**
    /// while the regular 32-bit offset is right. The spec says the ZIP64 value only applies when
    /// the 32-bit field is `0xFFFFFFFF`; zip 2.x used it anyway and rejected the whole package
    /// as "Could not find EOCD" (owner-reported on a real Bambu file, 2026-11-23).
    fn stale_zip64_package(files: &[(&str, &[u8])]) -> Vec<u8> {
        fn crc32(data: &[u8]) -> u32 {
            let mut crc = !0u32;
            for &b in data {
                crc ^= b as u32;
                for _ in 0..8 {
                    crc = if crc & 1 != 0 {
                        (crc >> 1) ^ 0xEDB8_8320
                    } else {
                        crc >> 1
                    };
                }
            }
            !crc
        }
        let (mut out, mut central) = (Vec::new(), Vec::new());
        for (name, data) in files {
            let offset = out.len() as u32;
            let (crc, len, name_len) = (crc32(data), data.len() as u32, name.len() as u16);
            out.extend(0x0403_4b50u32.to_le_bytes());
            for v in [45u16, 0, 0, 0, 0] {
                out.extend(v.to_le_bytes()); // version, flags, STORED, time, date
            }
            for v in [crc, len, len] {
                out.extend(v.to_le_bytes());
            }
            out.extend(name_len.to_le_bytes());
            out.extend(0u16.to_le_bytes());
            out.extend(name.as_bytes());
            out.extend(*data);

            central.extend(0x0201_4b50u32.to_le_bytes());
            for v in [45u16, 45, 0, 0, 0, 0] {
                central.extend(v.to_le_bytes());
            }
            for v in [crc, len, len] {
                central.extend(v.to_le_bytes());
            }
            for v in [name_len, 28, 0, 0, 0] {
                central.extend(v.to_le_bytes()); // name, extra, comment, disk, internal attrs
            }
            central.extend(0u32.to_le_bytes()); // external attrs
            central.extend(offset.to_le_bytes()); // the RIGHT offset
            central.extend(name.as_bytes());
            central.extend(1u16.to_le_bytes()); // ZIP64 extended information
            central.extend(24u16.to_le_bytes());
            central.extend(u64::from(len).to_le_bytes());
            central.extend(u64::from(len).to_le_bytes());
            central.extend((u64::from(offset) + 41_013).to_le_bytes()); // the STALE offset
        }
        let cd_offset = out.len() as u32;
        let cd_len = central.len() as u32;
        out.extend(central);
        out.extend(0x0605_4b50u32.to_le_bytes());
        let n = files.len() as u16;
        for v in [0u16, 0, n, n] {
            out.extend(v.to_le_bytes());
        }
        out.extend(cd_len.to_le_bytes());
        out.extend(cd_offset.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out
    }

    #[test]
    fn bambu_style_stale_zip64_offsets_still_open() {
        let xml = model_xml(&tetra(1, ""), r#"<item objectid="1"/>"#);
        let bytes = stale_zip64_package(&[
            ("_rels/.rels", RELS.as_bytes()),
            ("3D/3dmodel.model", xml.as_bytes()),
        ]);
        let m = parse_default(&bytes).expect("a valid 32-bit offset wins over a stale ZIP64 one");
        assert_eq!(m.triangles.len(), 4);
    }

    #[test]
    fn not_a_zip_is_malformed() {
        assert!(matches!(
            parse_default(b"solid x"),
            Err(MeshError::Malformed(_))
        ));
    }

    #[test]
    fn transform_composition_matches_sequential_application() {
        let a = Transform::parse("0 1 0 -1 0 0 0 0 1 1 2 3").unwrap(); // rotate + translate
        let b = Transform::parse("2 0 0 0 2 0 0 0 2 10 0 0").unwrap(); // scale + translate
        let p = [1.0, 2.0, 3.0];
        assert_eq!(a.then(&b).apply(p), b.apply(a.apply(p)));
        assert!(Transform::parse("1 0 0").is_none());
        assert!(Transform::parse("1 0 0 0 1 0 0 0 1 0 0 NaN").is_none());
    }
}
