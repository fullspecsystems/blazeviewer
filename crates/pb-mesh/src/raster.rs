//! The still renderer: a model → a straight-alpha RGBA8 picture, on the CPU.
//!
//! Behind [`MeshRasterizer`] so a GPU offscreen render can be A/B'd against it later (plan §
//! *Rendering the still*). The CPU path runs on the decode pool like any other decode, needs no
//! GPU device on worker threads, never contends with the presenter's queue mid-blaze, and is
//! deterministic for tests.
//!
//! The look, kept deliberately simple:
//! - **flat shading**: one colour per triangle, so the inner loop writes a constant colour + depth
//!   and lighting is computed once per triangle, not per pixel;
//! - normals **recomputed** from the geometry (STL's stored ones are often wrong) and flipped
//!   towards the camera, so open or inside-out meshes still read (two-sided lighting);
//! - a key light from upper-left, a fill from the right, and a sky/ground ambient, all
//!   relative to the camera, mixed in linear light;
//! - a **transparent background**, so the viewer's own backdrop shows, like a transparent PNG;
//! - the picture's aspect ratio follows the model's silhouette, so a long part isn't
//!   letterboxed into a square;
//! - 2×2 supersampling when the buffer fits a pixel budget. At large fits it renders at 1×:
//!   a parked, supersampled refinement rung is #151 follow-up.

use std::sync::OnceLock;

use crate::camera::{cross, dot, sub, Camera};
use crate::{MeshError, Model, DEFAULT_COLOR};

/// Fraction of the picture the silhouette fills along its limiting axis (the rest is margin).
/// Shared with the live view, which must frame the model exactly as the still did.
pub(crate) const FILL: f32 = 0.92;

// Lighting, in linear-light multiples of the material colour. The brightest a face can get is
// ~0.95, so a white filament still shows its shading instead of clipping flat (a Bambu
// calibration print colours its parts #F6F6F3). The live view's shader uses the same numbers
// (through `live::LiveFrame`), so the still → live hand-off doesn't change brightness.
pub(crate) const AMBIENT: f32 = 0.13;
pub(crate) const SKY: f32 = 0.10;
pub(crate) const KEY: f32 = 0.58;
pub(crate) const FILL_LIGHT: f32 = 0.22;

/// The key and fill light directions for a camera, in world space: camera-relative, so they
/// turn with the view.
pub(crate) fn light_dirs(cam: &Camera) -> ([f32; 3], [f32; 3]) {
    (
        cam.world_dir(-0.45, 0.65, 0.6),
        cam.world_dir(0.8, 0.1, 0.6),
    )
}

/// The model's silhouette on the image plane of `cam`, in tan units (x/z, y/z):
/// `(min_x, max_x, min_y, max_y)` over the vertices a triangle uses, or `None` when it has no
/// extent.
pub(crate) fn silhouette(model: &Model, proj: &[[f32; 3]]) -> Option<[f32; 4]> {
    let (mut min_x, mut max_x) = (f32::INFINITY, f32::NEG_INFINITY);
    let (mut min_y, mut max_y) = (f32::INFINITY, f32::NEG_INFINITY);
    for tri in &model.triangles {
        for &i in tri {
            let p = proj[i as usize];
            min_x = min_x.min(p[0]);
            max_x = max_x.max(p[0]);
            min_y = min_y.min(p[1]);
            max_y = max_y.max(p[1]);
        }
    }
    let span = (max_x - min_x).max(max_y - min_y);
    (span.is_finite() && span > 0.0).then_some([min_x, max_x, min_y, max_y])
}

/// Every vertex projected through `cam`: `(x/z, y/z, 1/z)`.
pub(crate) fn project(model: &Model, cam: &Camera) -> Vec<[f32; 3]> {
    model
        .positions
        .iter()
        .map(|&p| {
            let v = cam.view_space(p);
            let iz = 1.0 / v[2];
            [v[0] * iz, v[1] * iz, iz]
        })
        .collect()
}

/// The padded spans the framing divides by: a flat part seen exactly edge-on has zero extent on
/// one axis, so it gets a sliver.
pub(crate) fn padded_spans(sil: [f32; 4]) -> (f32, f32) {
    let [min_x, max_x, min_y, max_y] = sil;
    let span = (max_x - min_x).max(max_y - min_y);
    (
        (max_x - min_x).max(span * 1e-3),
        (max_y - min_y).max(span * 1e-3),
    )
}

/// How big a picture to render.
#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    /// The box the picture must fit; the output is at most this size, smaller on one axis.
    pub max_width: u32,
    pub max_height: u32,
    /// Supersample 2×2 only while the supersampled buffer stays within this many pixels. Each
    /// sample costs 8 bytes (colour + depth), so the default bounds a render's scratch at 64 MB.
    pub max_supersampled_pixels: u64,
    /// The colour (sRGB) of a model whose file colours nothing: the viewer's model colour
    /// setting. A file's own colours always win.
    pub base_color: [u8; 3],
}

impl RenderOptions {
    pub fn fit(max_width: u32, max_height: u32) -> RenderOptions {
        RenderOptions {
            max_width,
            max_height,
            max_supersampled_pixels: 8_000_000,
            base_color: DEFAULT_COLOR,
        }
    }
}

/// A rendered still: tightly packed, straight-alpha, sRGB-encoded RGBA8.
#[derive(Debug, Clone)]
pub struct Rendered {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// The rendering seam (plan § Architecture): CPU today, a GPU offscreen render is the A/B
/// alternative.
pub trait MeshRasterizer: Send + Sync {
    fn render(&self, model: &Model, opts: &RenderOptions) -> Result<Rendered, MeshError>;
    fn name(&self) -> &'static str;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CpuRasterizer;

impl MeshRasterizer for CpuRasterizer {
    fn name(&self) -> &'static str {
        "cpu"
    }

    fn render(&self, model: &Model, opts: &RenderOptions) -> Result<Rendered, MeshError> {
        let bounds = model.bounds().ok_or(MeshError::Empty)?;
        let cam = Camera::framing(&bounds);

        // Project every vertex once: (x/z, y/z, 1/z). Perspective-correct depth interpolates
        // 1/z linearly in screen space. Only referenced vertices are ever read back, and the
        // framing guarantees those have positive depth.
        let proj = project(model, &cam);
        let sil = silhouette(model, &proj).ok_or(MeshError::Empty)?;
        let [min_x, max_x, min_y, max_y] = sil;
        let (span_x, span_y) = padded_spans(sil);

        let box_w = opts.max_width.max(1);
        let box_h = opts.max_height.max(1);
        let s = (box_w as f32 * FILL / span_x).min(box_h as f32 * FILL / span_y);
        let out_w = ((span_x * s / FILL).ceil() as u32).clamp(1, box_w);
        let out_h = ((span_y * s / FILL).ceil() as u32).clamp(1, box_h);
        let ss: u32 = if (out_w as u64 * 2) * (out_h as u64 * 2) <= opts.max_supersampled_pixels {
            2
        } else {
            1
        };
        let (rw, rh) = (out_w * ss, out_h * ss);
        let k = s * ss as f32;
        // Centre the real extent (not the sliver-padded one) in the picture.
        let off_x = (rw as f32 - (max_x - min_x) * k) * 0.5;
        let off_y = (rh as f32 - (max_y - min_y) * k) * 0.5;
        let to_screen = |p: [f32; 3]| Vtx {
            x: (p[0] - min_x) * k + off_x,
            y: (max_y - p[1]) * k + off_y,
            iz: p[2],
        };

        let (key, fill) = light_dirs(&cam);
        let mut target = Target {
            w: rw as usize,
            h: rh as usize,
            depth: vec![0.0; rw as usize * rh as usize],
            color: vec![0; rw as usize * rh as usize],
        };
        for (ti, tri) in model.triangles.iter().enumerate() {
            let [a, b, c] = tri.map(|i| model.positions[i as usize]);
            let n = cross(sub(b, a), sub(c, a));
            let len = dot(n, n).sqrt();
            if !(len > 0.0 && len.is_finite()) {
                continue; // degenerate: no area, nothing to draw
            }
            let mut n = [n[0] / len, n[1] / len, n[2] / len];
            if dot(n, sub(cam.eye, a)) < 0.0 {
                n = [-n[0], -n[1], -n[2]];
            }
            let base = model
                .triangle_colors
                .as_ref()
                .map_or(opts.base_color, |c| c[ti]);
            let rgba = shade(base, n, key, fill);
            let [pa, pb, pc] = tri.map(|i| to_screen(proj[i as usize]));
            target.triangle(pa, pb, pc, rgba);
        }

        Ok(Rendered {
            rgba: target.resolve(ss as usize),
            width: out_w,
            height: out_h,
        })
    }
}

#[derive(Clone, Copy)]
struct Vtx {
    x: f32,
    y: f32,
    iz: f32,
}

/// The edge function: twice the signed area of (a, b, p).
#[inline]
fn edge(a: Vtx, b: Vtx, px: f32, py: f32) -> f32 {
    (b.x - a.x) * (py - a.y) - (b.y - a.y) * (px - a.x)
}

struct Target {
    w: usize,
    h: usize,
    /// 1/z per sample; larger is nearer; 0 is empty.
    depth: Vec<f32>,
    /// Packed RGBA per sample; 0 is transparent background.
    color: Vec<u32>,
}

impl Target {
    /// Half-space rasterization with incremental edge functions, sampling at pixel centres.
    fn triangle(&mut self, v0: Vtx, mut v1: Vtx, mut v2: Vtx, rgba: u32) {
        let mut area = edge(v0, v1, v2.x, v2.y);
        if !(area.abs() > 1e-12 && area.is_finite()) {
            return;
        }
        if area < 0.0 {
            std::mem::swap(&mut v1, &mut v2);
            area = -area;
        }
        let min_x = v0.x.min(v1.x).min(v2.x).floor().max(0.0);
        let max_x = v0.x.max(v1.x).max(v2.x).ceil().min(self.w as f32 - 1.0);
        let min_y = v0.y.min(v1.y).min(v2.y).floor().max(0.0);
        let max_y = v0.y.max(v1.y).max(v2.y).ceil().min(self.h as f32 - 1.0);
        if min_x > max_x || min_y > max_y {
            return;
        }
        let (x0, x1) = (min_x as usize, max_x as usize);
        let (y0, y1) = (min_y as usize, max_y as usize);
        let inv_area = 1.0 / area;
        // d(edge)/dx for each edge, opposite v0, v1, v2 respectively.
        let (d0, d1, d2) = (-(v2.y - v1.y), -(v0.y - v2.y), -(v1.y - v0.y));
        for y in y0..=y1 {
            let py = y as f32 + 0.5;
            let px = x0 as f32 + 0.5;
            let mut e0 = edge(v1, v2, px, py);
            let mut e1 = edge(v2, v0, px, py);
            let mut e2 = edge(v0, v1, px, py);
            // Solve each edge for the run of x where it is non-negative, so a long thin
            // triangle walks its covered span rather than its whole bounding box (a fan of
            // wedges otherwise costs O(triangles × bbox area)). The span is widened by one
            // pixel each side and the exact test below stays authoritative.
            let (mut lo, mut hi) = (0i64, (x1 - x0) as i64);
            for (e, d) in [(e0, d0), (e1, d1), (e2, d2)] {
                if d > 0.0 {
                    lo = lo.max((-e / d).floor() as i64 - 1);
                } else if d < 0.0 {
                    hi = hi.min((e / -d).ceil() as i64 + 1);
                } else if e < 0.0 {
                    lo = 1;
                    hi = 0;
                }
            }
            if lo > hi {
                continue;
            }
            let (lo, hi) = (lo as usize, hi as usize);
            e0 += d0 * lo as f32;
            e1 += d1 * lo as f32;
            e2 += d2 * lo as f32;
            let row = y * self.w;
            for x in x0 + lo..=x0 + hi {
                if e0 >= 0.0 && e1 >= 0.0 && e2 >= 0.0 {
                    let iz = (e0 * v0.iz + e1 * v1.iz + e2 * v2.iz) * inv_area;
                    let i = row + x;
                    if iz > self.depth[i] {
                        self.depth[i] = iz;
                        self.color[i] = rgba;
                    }
                }
                e0 += d0;
                e1 += d1;
                e2 += d2;
            }
        }
    }

    /// Box-filter `ss`×`ss` samples into straight-alpha RGBA8: colour averages the covered
    /// samples only, alpha is the coverage.
    fn resolve(self, ss: usize) -> Vec<u8> {
        let (ow, oh) = (self.w / ss, self.h / ss);
        let mut out = vec![0u8; ow * oh * 4];
        if ss == 1 {
            for (px, c) in out.chunks_exact_mut(4).zip(&self.color) {
                px.copy_from_slice(&c.to_le_bytes());
            }
            return out;
        }
        for oy in 0..oh {
            for ox in 0..ow {
                let (mut r, mut g, mut b, mut covered) = (0u32, 0u32, 0u32, 0u32);
                for sy in 0..ss {
                    for sx in 0..ss {
                        let c = self.color[(oy * ss + sy) * self.w + ox * ss + sx].to_le_bytes();
                        if c[3] != 0 {
                            r += c[0] as u32;
                            g += c[1] as u32;
                            b += c[2] as u32;
                            covered += 1;
                        }
                    }
                }
                if let Some(cnt) = std::num::NonZeroU32::new(covered) {
                    let o = (oy * ow + ox) * 4;
                    let n = (ss * ss) as u32;
                    let half = covered / 2;
                    out[o] = ((r + half) / cnt) as u8;
                    out[o + 1] = ((g + half) / cnt) as u8;
                    out[o + 2] = ((b + half) / cnt) as u8;
                    out[o + 3] = ((covered * 255 + n / 2) / n) as u8;
                }
            }
        }
        out
    }
}

/// Light one triangle: the material colour times the light reaching a face with normal `n`,
/// in linear light, packed as opaque sRGB RGBA.
fn shade(base: [u8; 3], n: [f32; 3], key: [f32; 3], fill: [f32; 3]) -> u32 {
    let sky = 0.5 + 0.5 * n[2];
    let light =
        AMBIENT + SKY * sky + KEY * dot(n, key).max(0.0) + FILL_LIGHT * dot(n, fill).max(0.0);
    let decode = srgb_to_linear_lut();
    let c = base.map(|v| linear_to_srgb(decode[v as usize] * light));
    u32::from_le_bytes([c[0], c[1], c[2], 255])
}

fn srgb_to_linear_lut() -> &'static [f32; 256] {
    static LUT: OnceLock<[f32; 256]> = OnceLock::new();
    LUT.get_or_init(|| {
        std::array::from_fn(|i| {
            let c = i as f32 / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        })
    })
}

fn linear_to_srgb(v: f32) -> u8 {
    const N: usize = 4096;
    static LUT: OnceLock<Vec<u8>> = OnceLock::new();
    let lut = LUT.get_or_init(|| {
        (0..N)
            .map(|i| {
                let l = i as f32 / (N - 1) as f32;
                let s = if l <= 0.003_130_8 {
                    l * 12.92
                } else {
                    1.055 * l.powf(1.0 / 2.4) - 0.055
                };
                (s * 255.0).round().clamp(0.0, 255.0) as u8
            })
            .collect()
    });
    lut[((v.clamp(0.0, 1.0) * (N - 1) as f32) + 0.5) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse, test_util::cube_stl_binary, Limits, ModelFormat};

    fn cube() -> Model {
        parse(ModelFormat::Stl, &cube_stl_binary(), &Limits::default()).unwrap()
    }

    fn px(r: &Rendered, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * r.width + x) * 4) as usize;
        [r.rgba[i], r.rgba[i + 1], r.rgba[i + 2], r.rgba[i + 3]]
    }

    #[test]
    fn cube_fits_the_box_with_a_transparent_margin_and_an_opaque_centre() {
        let r = CpuRasterizer
            .render(&cube(), &RenderOptions::fit(400, 300))
            .unwrap();
        assert!(r.width <= 400 && r.height <= 300);
        assert!(r.width == 400 || r.height == 300, "fills the limiting axis");
        assert_eq!(r.rgba.len(), (r.width * r.height * 4) as usize);
        assert_eq!(px(&r, 0, 0)[3], 0, "corner is background");
        assert_eq!(px(&r, r.width / 2, r.height / 2)[3], 255, "centre is model");
    }

    #[test]
    fn three_visible_faces_get_three_shades() {
        let r = CpuRasterizer
            .render(&cube(), &RenderOptions::fit(256, 256))
            .unwrap();
        let mut counts = std::collections::HashMap::<[u8; 3], usize>::new();
        let opaque = r.rgba.chunks_exact(4).filter(|p| p[3] == 255);
        for p in opaque.clone() {
            *counts.entry([p[0], p[1], p[2]]).or_default() += 1;
        }
        let total = opaque.count();
        let mut top: Vec<usize> = counts.into_values().collect();
        top.sort_unstable_by(|a, b| b.cmp(a));
        // Three faces, each a real share of the picture, and distinct from each other;
        // the remainder is the antialiased seams between them.
        assert!(top.len() >= 3);
        assert!(top[..3].iter().all(|&c| c * 20 >= total), "{top:?}");
        assert!(top[..3].iter().sum::<usize>() * 10 >= total * 9, "{top:?}");
    }

    #[test]
    fn rendering_is_deterministic() {
        let a = CpuRasterizer
            .render(&cube(), &RenderOptions::fit(200, 200))
            .unwrap();
        let b = CpuRasterizer
            .render(&cube(), &RenderOptions::fit(200, 200))
            .unwrap();
        assert_eq!(a.rgba, b.rgba);
    }

    #[test]
    fn file_colours_are_used() {
        let mut m = cube();
        m.triangle_colors = Some(vec![[220, 30, 30]; m.triangles.len()]);
        let r = CpuRasterizer
            .render(&m, &RenderOptions::fit(128, 128))
            .unwrap();
        let c = px(&r, r.width / 2, r.height / 2);
        assert!(c[0] > c[1] * 2 && c[0] > c[2] * 2, "red dominates: {c:?}");
    }

    #[test]
    fn an_uncoloured_model_takes_the_viewers_colour_but_file_colours_win() {
        let mut opts = RenderOptions::fit(128, 128);
        opts.base_color = [30, 30, 220];
        let mut m = cube();
        let r = CpuRasterizer.render(&m, &opts).unwrap();
        let c = px(&r, r.width / 2, r.height / 2);
        assert!(c[2] > c[0] * 2 && c[2] > c[1] * 2, "blue dominates: {c:?}");
        m.triangle_colors = Some(vec![[220, 30, 30]; m.triangles.len()]);
        let r = CpuRasterizer.render(&m, &opts).unwrap();
        let c = px(&r, r.width / 2, r.height / 2);
        assert!(c[0] > c[2] * 2, "the file's red wins: {c:?}");
    }

    #[test]
    fn large_fits_skip_supersampling_but_still_render() {
        let mut opts = RenderOptions::fit(300, 300);
        opts.max_supersampled_pixels = 0;
        let r = CpuRasterizer.render(&cube(), &opts).unwrap();
        // At 1× every sample is fully in or out: no partial alpha anywhere.
        assert!(r.rgba.chunks_exact(4).all(|p| p[3] == 0 || p[3] == 255));
        assert!(r.rgba.chunks_exact(4).any(|p| p[3] == 255));
    }

    #[test]
    fn a_long_part_is_not_letterboxed_into_a_square() {
        let mut m = cube();
        for p in &mut m.positions {
            p[0] *= 10.0;
        }
        let r = CpuRasterizer
            .render(&m, &RenderOptions::fit(1000, 1000))
            .unwrap();
        // Seen at the 3/4 angle a 10:1 bar projects to roughly 2:1 — the point is that the
        // picture follows it rather than padding out to the square box.
        assert!(
            r.width == 1000 && r.height < 600,
            "{}x{}",
            r.width,
            r.height
        );
    }

    #[test]
    fn empty_model_is_an_error() {
        let mut m = cube();
        m.triangles.clear();
        assert!(matches!(
            CpuRasterizer.render(&m, &RenderOptions::fit(64, 64)),
            Err(MeshError::Empty)
        ));
    }
}
