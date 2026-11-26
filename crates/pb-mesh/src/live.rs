//! The live 3D view's math (task #153): a turntable camera that starts at the still's exact
//! pose **and** framing, and the per-frame values a GPU mesh pass needs.
//!
//! Pure, like the rest of the crate: `pb-render` draws, `pb-app-core` owns the state and turns
//! input into calls here.
//!
//! **The hand-off rule.** At home pose and zoom 1, [`LiveView::frame`] puts every vertex on the
//! same pixel the CPU still put it on (when the still was rendered for a box with the
//! viewport's aspect and shown fitted, which is how the viewer shows it). The projection is
//! the still's: tan-space `(x/z, y/z)`, centred on the silhouette's centre, scaled so the
//! silhouette fills [`FILL`] of the limiting axis. Zoom scales that, pan shifts the centre, and
//! orbiting turns the camera around the model's centre while the projection stays put. Swapping
//! the picture for the live view therefore doesn't move anything.

use std::f32::consts::{FRAC_PI_2, PI};

use crate::camera::{dot, Camera, Orbit};
use crate::raster::{
    light_dirs, padded_spans, project, silhouette, AMBIENT, FILL, FILL_LIGHT, KEY, SKY,
};
use crate::Model;

/// Elevation stops just short of straight up/down, where a turntable's basis degenerates.
const MAX_ELEVATION: f32 = FRAC_PI_2 - 0.01;
/// Zoom range, as a multiple of the framed (home) size.
const MIN_ZOOM: f32 = 0.1;
const MAX_ZOOM: f32 = 50.0;
/// A drag across the viewport's shorter side turns the model this far (radians), so the feel is
/// the same on any display and at any scale factor.
const ORBIT_PER_SHORT_SIDE: f32 = PI;

/// The live camera: an orbit around the model's centre plus the still's projection, zoomed and
/// panned.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LiveView {
    orbit: Orbit,
    home: Orbit,
    /// The home silhouette's centre in tan units: the projection's principal point.
    centre: [f32; 2],
    home_centre: [f32; 2],
    /// The home silhouette's padded spans in tan units, which set the fit scale.
    span: [f32; 2],
    zoom: f32,
    /// Radians the picture is turned about the line of sight, counterclockwise on screen, in
    /// `[0, 2π)`: a trackpad twist. It never changes which way is up for the turntable.
    roll: f32,
}

/// Everything a GPU mesh pass needs for one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LiveFrame {
    /// World → clip, **column-major** (WGSL `mat4x4<f32>` layout). Depth lands in `[0, 1]`,
    /// nearer is smaller.
    pub view_proj: [[f32; 4]; 4],
    /// The camera position in world space (for the two-sided normal flip).
    pub eye: [f32; 3],
    /// Key and fill light directions in world space (unit vectors towards the light).
    pub key_dir: [f32; 3],
    pub fill_dir: [f32; 3],
    /// `[ambient, sky, key, fill]` strengths: the CPU still's lighting, verbatim.
    pub lighting: [f32; 4],
}

impl LiveView {
    /// The home view for `model`, or `None` for a model with nothing to draw.
    pub fn new(model: &Model) -> Option<LiveView> {
        let bounds = model.bounds()?;
        let home = Orbit::home(&bounds);
        let proj = project(model, &home.camera());
        let sil = silhouette(model, &proj)?;
        let (span_x, span_y) = padded_spans(sil);
        let centre = [(sil[0] + sil[1]) * 0.5, (sil[2] + sil[3]) * 0.5];
        Some(LiveView {
            orbit: home,
            home,
            centre,
            home_centre: centre,
            span: [span_x, span_y],
            zoom: 1.0,
            roll: 0.0,
        })
    }

    /// Turn the model by a pointer drag of `(dx, dy)` pixels in a `viewport` (y down). Dragging
    /// right turns the model to the right; dragging down tips its top towards you. On a rolled
    /// model those are the model's own right and down as they appear on screen, so it still
    /// turns the way the pointer moves.
    pub fn orbit_by(&mut self, dx: f32, dy: f32, viewport: [f32; 2]) {
        let short = viewport[0].min(viewport[1]).max(1.0);
        let per_px = ORBIT_PER_SHORT_SIDE / short;
        if !(dx.is_finite() && dy.is_finite()) {
            return;
        }
        let [dx, dy] = self.unroll([dx, dy]);
        self.orbit.azimuth = (self.orbit.azimuth - dx * per_px).rem_euclid(2.0 * PI);
        self.orbit.elevation =
            (self.orbit.elevation + dy * per_px).clamp(-MAX_ELEVATION, MAX_ELEVATION);
    }

    /// Turn the model about its vertical axis by `radians`: the turntable (`P`). Positive turns
    /// it the way a drag to the right does. Elevation, zoom and pan are left alone, so a spin
    /// keeps whatever view the user set up.
    pub fn spin_by(&mut self, radians: f32) {
        if radians.is_finite() {
            self.orbit.azimuth = (self.orbit.azimuth - radians).rem_euclid(2.0 * PI);
        }
    }

    /// Turn the picture `radians` about the model's centre on screen, counterclockwise for a
    /// positive angle: a trackpad twist, so an upside-down model can be stood the right way up.
    /// Like zoom and pan, it leaves the turntable's axis alone.
    pub fn roll_by(&mut self, radians: f32) {
        if radians.is_finite() {
            self.roll = (self.roll + radians).rem_euclid(2.0 * PI);
        }
    }

    /// Roll by a sideways drag of `dx` pixels (Option+drag): right turns the model clockwise, as
    /// if pushing its top, at the rate a drag turns it.
    pub fn roll_by_drag(&mut self, dx: f32, viewport: [f32; 2]) {
        let short = viewport[0].min(viewport[1]).max(1.0);
        self.roll_by(-dx * ORBIT_PER_SHORT_SIDE / short);
    }

    /// A glide's screen velocity with any push past the pole the camera rests on taken out, so
    /// the glide stops there rather than grinding on invisibly, and keeps the rest of its motion.
    pub fn stop_at_poles(&self, v: [f32; 2]) -> [f32; 2] {
        let [turn, tip] = self.unroll(v);
        let e = self.orbit.elevation;
        if (e >= MAX_ELEVATION && tip > 0.0) || (e <= -MAX_ELEVATION && tip < 0.0) {
            self.reroll([turn, 0.0])
        } else {
            v
        }
    }

    /// A screen vector (y down) in the unrolled picture's terms: what it would be had the
    /// picture not been rolled.
    fn unroll(&self, [x, y]: [f32; 2]) -> [f32; 2] {
        let (s, c) = self.roll.sin_cos();
        [x * c - y * s, x * s + y * c]
    }

    /// The inverse of [`unroll`](Self::unroll).
    fn reroll(&self, [x, y]: [f32; 2]) -> [f32; 2] {
        let (s, c) = self.roll.sin_cos();
        [x * c + y * s, y * c - x * s]
    }

    /// Zoom by `factor` (> 1 is closer) about the viewport's centre.
    pub fn zoom_by(&mut self, factor: f32) {
        if factor.is_finite() && factor > 0.0 {
            self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        }
    }

    /// Zoom by `factor` keeping the point under `cursor` (pixels from the viewport's top-left)
    /// still: what scroll and pinch want.
    pub fn zoom_about(&mut self, factor: f32, cursor: [f32; 2], viewport: [f32; 2]) {
        if !(factor.is_finite() && factor > 0.0 && cursor.iter().all(|c| c.is_finite())) {
            return;
        }
        let k = self.pixels_per_unit(viewport);
        let off = [cursor[0] - viewport[0] * 0.5, cursor[1] - viewport[1] * 0.5];
        let under = [self.centre[0] + off[0] / k, self.centre[1] - off[1] / k];
        self.zoom_by(factor);
        let k2 = self.pixels_per_unit(viewport);
        self.centre = [under[0] - off[0] / k2, under[1] + off[1] / k2];
    }

    /// Slide the model by a drag of `(dx, dy)` pixels (y down).
    pub fn pan_by(&mut self, dx: f32, dy: f32, viewport: [f32; 2]) {
        if dx.is_finite() && dy.is_finite() {
            let k = self.pixels_per_unit(viewport);
            self.centre[0] -= dx / k;
            self.centre[1] += dy / k;
        }
    }

    /// Back to the still's pose and framing.
    pub fn reset(&mut self) {
        self.orbit = self.home;
        self.centre = self.home_centre;
        self.zoom = 1.0;
        self.roll = 0.0;
    }

    /// Whether the view is exactly the still's: the moment the live view and the picture agree.
    pub fn is_home(&self) -> bool {
        self.orbit == self.home
            && self.centre == self.home_centre
            && self.zoom == 1.0
            && self.roll == 0.0
    }

    /// Current zoom as a multiple of the framed size.
    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    /// Pixels per tan unit in a `viewport`: the still's fit scale times the zoom.
    fn pixels_per_unit(&self, viewport: [f32; 2]) -> f32 {
        let fit = (viewport[0].max(1.0) * FILL / self.span[0])
            .min(viewport[1].max(1.0) * FILL / self.span[1]);
        fit * self.zoom
    }

    /// The uniforms for drawing one frame into a `viewport` (pixels).
    pub fn frame(&self, viewport: [f32; 2]) -> LiveFrame {
        let cam = self.camera();
        let k = self.pixels_per_unit(viewport);
        let kx = 2.0 * k / viewport[0].max(1.0);
        let ky = 2.0 * k / viewport[1].max(1.0);
        let [cx, cy] = self.centre;
        // The camera never comes closer than its home distance, so the sphere is always in front.
        let margin = self.orbit.radius * 1.05;
        let near = (self.orbit.distance - margin).max(self.orbit.distance * 1e-3);
        let far = self.orbit.distance + margin;
        let depth_scale = far / (far - near);

        // view space: (right·d, up·d, fwd·d) with d = p − eye.
        let view = [
            row(cam.right, cam.eye),
            row(cam.up, cam.eye),
            row(cam.fwd, cam.eye),
            [0.0, 0.0, 0.0, 1.0],
        ];
        // clip.x = (x − cx·z)·kx, clip.y = (y − cy·z)·ky, clip.z = (z − near)·far/(far − near),
        // clip.w = z: the still's tan-space mapping, with a [0, 1] depth.
        let proj = [
            [kx, 0.0, -cx * kx, 0.0],
            [0.0, ky, -cy * ky, 0.0],
            [0.0, 0.0, depth_scale, -near * depth_scale],
            [0.0, 0.0, 1.0, 0.0],
        ];
        let m = mul(proj, view);
        let (key_dir, fill_dir) = light_dirs(&cam);
        LiveFrame {
            // Transpose into column-major.
            view_proj: std::array::from_fn(|c| std::array::from_fn(|r| m[r][c])),
            eye: cam.eye,
            key_dir,
            fill_dir,
            lighting: [AMBIENT, SKY, KEY, FILL_LIGHT],
        }
    }
}

impl LiveView {
    /// The orbit's camera turned about its line of sight by the roll. The target stays on that
    /// line, so the model turns about its own centre on screen. The lights are camera-relative,
    /// so they stay put on screen like a lamp while the model turns under them.
    fn camera(&self) -> Camera {
        let mut cam = self.orbit.camera();
        let (s, c) = self.roll.sin_cos();
        let (right, up) = (cam.right, cam.up);
        // Image coordinates (right·d, up·d) turn counterclockwise by the roll.
        cam.right = std::array::from_fn(|i| c * right[i] - s * up[i]);
        cam.up = std::array::from_fn(|i| s * right[i] + c * up[i]);
        cam
    }
}

/// One view-matrix row: `axis · (p − eye)` as `[axis, −axis·eye]`.
fn row(axis: [f32; 3], eye: [f32; 3]) -> [f32; 4] {
    [axis[0], axis[1], axis[2], -dot(axis, eye)]
}

fn mul(a: [[f32; 4]; 4], b: [[f32; 4]; 4]) -> [[f32; 4]; 4] {
    std::array::from_fn(|r| std::array::from_fn(|c| (0..4).map(|k| a[r][k] * b[k][c]).sum()))
}

/// The model as a GPU-ready triangle soup: three corners per triangle, each with its
/// triangle's sRGB colour, or `None` for a model whose file colours nothing (it is drawn in the
/// viewer's model colour, which the GPU applies per frame, so changing it never re-uploads). A
/// soup, not an indexed mesh, because colour is per triangle; flat normals come from
/// screen-space derivatives in the shader.
pub fn corners(model: &Model) -> impl Iterator<Item = ([f32; 3], Option<[u8; 3]>)> + '_ {
    model
        .triangles
        .iter()
        .enumerate()
        .flat_map(move |(ti, tri)| {
            let color = model.triangle_colors.as_ref().map(|c| c[ti]);
            tri.map(|i| (model.positions[i as usize], color))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raster::{
        render_view_banded, CpuRasterizer, MeshRasterizer, RenderOptions, Rendered,
    };
    use crate::DEFAULT_COLOR;
    use crate::{parse, test_util::cube_stl_binary, Limits, ModelFormat};

    fn cube() -> Model {
        parse(ModelFormat::Stl, &cube_stl_binary(), &Limits::default()).unwrap()
    }

    /// Clip → pixel (y down) for a world point.
    fn to_px(f: &LiveFrame, p: [f32; 3], vp: [f32; 2]) -> [f32; 2] {
        let m = f.view_proj;
        let c: [f32; 4] =
            std::array::from_fn(|r| (0..3).map(|k| m[k][r] * p[k]).sum::<f32>() + m[3][r]);
        [
            (c[0] / c[3] * 0.5 + 0.5) * vp[0],
            (0.5 - c[1] / c[3] * 0.5) * vp[1],
        ]
    }

    #[test]
    fn home_frame_lands_where_the_still_drew() {
        // The still for a 400×300 box, shown fitted in a 400×300 viewport (1:1, centred), must
        // agree with the live frame on where the model's opaque pixels are.
        let m = cube();
        let vp = [400.0, 300.0];
        let still = CpuRasterizer
            .render(&m, &RenderOptions::fit(400, 300))
            .unwrap();
        let live = LiveView::new(&m).unwrap().frame(vp);
        // The still is centred in the viewport; find its opaque bounding box in viewport pixels.
        let (ox, oy) = (
            (vp[0] - still.width as f32) / 2.0,
            (vp[1] - still.height as f32) / 2.0,
        );
        let (mut min, mut max) = ([f32::MAX; 2], [f32::MIN; 2]);
        for y in 0..still.height {
            for x in 0..still.width {
                if still.rgba[((y * still.width + x) * 4 + 3) as usize] > 127 {
                    min = [min[0].min(x as f32 + ox), min[1].min(y as f32 + oy)];
                    max = [
                        max[0].max(x as f32 + 1.0 + ox),
                        max[1].max(y as f32 + 1.0 + oy),
                    ];
                }
            }
        }
        // The cube's 8 corners through the live projection span the same box (within a pixel).
        let (mut lmin, mut lmax) = ([f32::MAX; 2], [f32::MIN; 2]);
        for i in 0..8 {
            let p = [(i & 1) as f32, ((i >> 1) & 1) as f32, ((i >> 2) & 1) as f32];
            let q = to_px(&live, p, vp);
            lmin = [lmin[0].min(q[0]), lmin[1].min(q[1])];
            lmax = [lmax[0].max(q[0]), lmax[1].max(q[1])];
        }
        for a in 0..2 {
            assert!((min[a] - lmin[a]).abs() <= 1.5, "min {min:?} vs {lmin:?}");
            assert!((max[a] - lmax[a]).abs() <= 1.5, "max {max:?} vs {lmax:?}");
        }
    }

    fn alpha(r: &Rendered, x: u32, y: u32) -> u8 {
        r.rgba[((y * r.width + x) * 4 + 3) as usize]
    }

    fn opaque(r: &Rendered) -> usize {
        r.rgba.chunks_exact(4).filter(|p| p[3] > 127).count()
    }

    /// Export and Copy of a turned model render the live camera on the CPU. At home it must
    /// agree with the still (shown fitted and centred) on coverage and shading, so a capture
    /// looks like the screen.
    #[test]
    fn the_cpu_view_render_matches_the_still_at_home() {
        let m = cube();
        let (w, h) = (400u32, 300u32);
        let still = CpuRasterizer.render(&m, &RenderOptions::fit(w, h)).unwrap();
        let frame = LiveView::new(&m).unwrap().frame([w as f32, h as f32]);
        let view = CpuRasterizer
            .render_view(&m, &frame, w, h, DEFAULT_COLOR)
            .unwrap();
        assert_eq!(
            (view.width, view.height),
            (w, h),
            "the window's shape and size"
        );
        let (ox, oy) = ((w - still.width) / 2, (h - still.height) / 2);
        let (mut covered, mut differ) = (0, 0);
        for y in 0..h {
            for x in 0..w {
                let inside = x >= ox && y >= oy && x - ox < still.width && y - oy < still.height;
                let s = if inside {
                    alpha(&still, x - ox, y - oy)
                } else {
                    0
                };
                let v = alpha(&view, x, y);
                if s > 127 || v > 127 {
                    covered += 1;
                    differ += usize::from((s > 127) != (v > 127));
                }
            }
        }
        assert!(covered > 10_000, "{covered}");
        assert!(
            differ * 50 < covered,
            "{differ} of {covered} pixels disagree"
        );
        let at = |r: &Rendered, x: u32, y: u32| {
            let i = ((y * r.width + x) * 4) as usize;
            [r.rgba[i], r.rgba[i + 1], r.rgba[i + 2]]
        };
        let (a, b) = (at(&view, w / 2, h / 2), at(&still, w / 2 - ox, h / 2 - oy));
        for c in 0..3 {
            assert!(a[c].abs_diff(b[c]) <= 3, "shading {a:?} vs {b:?}");
        }
    }

    #[test]
    fn the_cpu_view_render_follows_the_camera_and_the_colour() {
        let m = cube();
        let vp = [320.0, 240.0];
        let render = |v: &LiveView, color| {
            CpuRasterizer
                .render_view(&m, &v.frame(vp), 320, 240, color)
                .unwrap()
        };
        let mut v = LiveView::new(&m).unwrap();
        let home = render(&v, DEFAULT_COLOR);
        v.orbit_by(90.0, 30.0, vp);
        let turned = render(&v, DEFAULT_COLOR);
        assert_ne!(home.rgba, turned.rgba, "turning changes the picture");
        v.zoom_by(2.0);
        assert!(
            opaque(&render(&v, DEFAULT_COLOR)) > opaque(&turned) * 2,
            "zoom fills more"
        );
        let green = render(&v, [30, 200, 40]);
        let i = ((120 * 320 + 160) * 4) as usize;
        let [r, g, b] = [green.rgba[i], green.rgba[i + 1], green.rgba[i + 2]];
        assert!(g > r * 2 && g > b * 2, "the model colour: {r} {g} {b}");
    }

    /// A big view renders in bands to bound its scratch memory; the bands must not show.
    #[test]
    fn rendering_in_bands_changes_nothing() {
        let m = cube();
        let mut v = LiveView::new(&m).unwrap();
        v.orbit_by(40.0, -20.0, [320.0, 240.0]);
        let f = v.frame([320.0, 240.0]);
        let whole = render_view_banded(&m, &f, 320, 240, DEFAULT_COLOR, usize::MAX).unwrap();
        let banded = render_view_banded(&m, &f, 320, 240, DEFAULT_COLOR, 640 * 6).unwrap();
        assert_eq!(whole.rgba, banded.rgba);
    }

    #[test]
    fn depth_is_in_range_and_ordered() {
        let m = cube();
        let f = LiveView::new(&m).unwrap().frame([800.0, 600.0]);
        let depth = |p: [f32; 3]| {
            let v = f.view_proj;
            let z: f32 = (0..3).map(|k| v[k][2] * p[k]).sum::<f32>() + v[3][2];
            let w: f32 = (0..3).map(|k| v[k][3] * p[k]).sum::<f32>() + v[3][3];
            z / w
        };
        // The corner nearest the camera (front-right-top) is nearer than the far one.
        let (near, far) = (depth([1.0, 0.0, 1.0]), depth([0.0, 1.0, 0.0]));
        assert!((0.0..=1.0).contains(&near) && (0.0..=1.0).contains(&far));
        assert!(near < far);
    }

    #[test]
    fn orbit_zoom_pan_and_reset() {
        let mut v = LiveView::new(&cube()).unwrap();
        let vp = [800.0, 600.0];
        assert!(v.is_home());
        v.orbit_by(120.0, -40.0, vp);
        v.zoom_by(2.0);
        v.pan_by(10.0, 5.0, vp);
        assert!(!v.is_home());
        assert_eq!(v.zoom(), 2.0);
        v.reset();
        assert!(v.is_home());
        // Zoom is clamped both ways.
        v.zoom_by(1e9);
        assert_eq!(v.zoom(), MAX_ZOOM);
        v.zoom_by(1e-12);
        assert_eq!(v.zoom(), MIN_ZOOM);
        // Garbage input is ignored, never poisons the view.
        v.reset();
        v.orbit_by(f32::NAN, 1.0, vp);
        v.zoom_by(f32::INFINITY);
        v.pan_by(f32::NAN, 0.0, vp);
        assert!(v.is_home());
    }

    #[test]
    fn zoom_about_keeps_the_cursor_point_still() {
        let m = cube();
        let vp = [800.0, 600.0];
        let mut v = LiveView::new(&m).unwrap();
        let p = [1.0, 0.0, 1.0];
        let before = to_px(&v.frame(vp), p, vp);
        v.zoom_about(3.0, before, vp);
        let after = to_px(&v.frame(vp), p, vp);
        assert!((before[0] - after[0]).abs() < 0.01 && (before[1] - after[1]).abs() < 0.01);
    }

    #[test]
    fn spinning_turns_like_a_drag_to_the_right_and_keeps_the_rest() {
        let vp = [800.0, 600.0];
        let mut dragged = LiveView::new(&cube()).unwrap();
        dragged.zoom_by(2.0);
        let mut spun = dragged;
        // A drag across the short side (600 px) is half a turn.
        dragged.orbit_by(150.0, 0.0, vp);
        spun.spin_by(PI / 4.0);
        assert!((dragged.orbit.azimuth - spun.orbit.azimuth).abs() < 1e-5);
        assert_eq!(spun.orbit.elevation, dragged.orbit.elevation);
        assert_eq!(spun.zoom(), 2.0);
        // A whole turn comes back round; nonsense is ignored.
        let before = spun.orbit.azimuth;
        spun.spin_by(2.0 * PI);
        assert!((spun.orbit.azimuth - before).abs() < 1e-4);
        spun.spin_by(f32::NAN);
        assert!(spun.orbit.azimuth.is_finite());
    }

    /// A roll turns the picture about the model's centre on screen (counterclockwise for a
    /// positive angle), wherever the model has been moved to. The camera stays where it is.
    #[test]
    fn rolling_turns_the_picture_about_the_models_centre() {
        let vp = [800.0, 600.0];
        let mut v = LiveView::new(&cube()).unwrap();
        v.pan_by(40.0, -20.0, vp); // off-centre: the pivot is the model, not the window
        let (centre, p) = (v.orbit.target, [1.0, 0.0, 1.0]);
        let f0 = v.frame(vp);
        let (c0, p0) = (to_px(&f0, centre, vp), to_px(&f0, p, vp));
        v.roll_by(FRAC_PI_2);
        assert!(!v.is_home());
        let f1 = v.frame(vp);
        let (c1, p1) = (to_px(&f1, centre, vp), to_px(&f1, p, vp));
        assert!((c0[0] - c1[0]).abs() < 0.01 && (c0[1] - c1[1]).abs() < 0.01);
        // A quarter turn counterclockwise on a y-down screen: (x, y) → (y, −x) about the centre.
        let d0 = [p0[0] - c0[0], p0[1] - c0[1]];
        let d1 = [p1[0] - c1[0], p1[1] - c1[1]];
        assert!(
            (d1[0] - d0[1]).abs() < 0.05 && (d1[1] + d0[0]).abs() < 0.05,
            "{d0:?} → {d1:?}"
        );
        assert_eq!(f1.eye, f0.eye, "the camera turned, it didn't move");
        v.reset();
        assert!(v.is_home());
        v.roll_by(f32::NAN);
        assert!(v.is_home());
    }

    /// Dragging right rolls the model clockwise, as if pushing its top, at the rate a drag turns
    /// it: half a turn across the viewport's short side.
    #[test]
    fn a_sideways_drag_rolls_clockwise() {
        let vp = [800.0, 600.0];
        let mut v = LiveView::new(&cube()).unwrap();
        v.roll_by_drag(300.0, vp);
        assert!((v.roll - 1.5 * PI).abs() < 1e-4, "a quarter turn clockwise");
        v.roll_by_drag(f32::NAN, vp);
        assert!(v.roll.is_finite());
    }

    /// Once rolled, a drag still turns the model the way the pointer moves on screen.
    #[test]
    fn a_drag_on_a_rolled_model_follows_the_pointer() {
        let vp = [800.0, 600.0];
        let mut rolled = LiveView::new(&cube()).unwrap();
        let mut plain = rolled;
        rolled.roll_by(FRAC_PI_2);
        // Rolled a quarter turn counterclockwise, the model's left is screen-down.
        rolled.orbit_by(0.0, 60.0, vp);
        plain.orbit_by(-60.0, 0.0, vp);
        assert!((rolled.orbit.azimuth - plain.orbit.azimuth).abs() < 1e-4);
        assert!((rolled.orbit.elevation - plain.orbit.elevation).abs() < 1e-4);
    }

    /// A glide stops pushing at a pole (the camera can't go over the top) and keeps the rest of
    /// its motion, whichever way the model is rolled.
    #[test]
    fn only_a_push_past_a_pole_is_stopped() {
        let mut v = LiveView::new(&cube()).unwrap();
        assert_eq!(
            v.stop_at_poles([0.0, 500.0]),
            [0.0, 500.0],
            "not at a pole yet"
        );
        v.orbit_by(0.0, 1e6, [800.0, 600.0]); // up against the top
        assert_eq!(v.stop_at_poles([300.0, 500.0]), [300.0, 0.0]);
        assert_eq!(
            v.stop_at_poles([0.0, -500.0]),
            [0.0, -500.0],
            "back down is fine"
        );
        // Rolled a quarter turn counterclockwise, screen-right is the push and down is a turn.
        v.roll_by(FRAC_PI_2);
        let [x, y] = v.stop_at_poles([500.0, 0.0]);
        assert!(x.abs() < 1e-3 && y.abs() < 1e-3, "{x}, {y}");
        let [x, y] = v.stop_at_poles([0.0, 500.0]);
        assert!(x.abs() < 1e-3 && (y - 500.0).abs() < 1e-3, "{x}, {y}");
    }

    #[test]
    fn elevation_never_flips_over_the_top() {
        let mut v = LiveView::new(&cube()).unwrap();
        v.orbit_by(0.0, 1e6, [800.0, 600.0]);
        let f = v.frame([800.0, 600.0]);
        assert!(f.view_proj.iter().flatten().all(|x| x.is_finite()));
    }

    #[test]
    fn corners_are_a_coloured_soup() {
        let mut m = cube();
        let c: Vec<_> = corners(&m).collect();
        assert_eq!(c.len(), 36);
        assert!(
            c.iter().all(|(_, col)| col.is_none()),
            "uncoloured: the viewer's colour"
        );
        m.triangle_colors = Some(vec![[1, 2, 3]; 12]);
        assert!(corners(&m).all(|(_, col)| col == Some([1, 2, 3])));
    }
}
