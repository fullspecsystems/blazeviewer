//! The still's camera: a 3/4 view from front-right-above, auto-framed to the model.
//!
//! World space is Z-up with the model's front facing −Y (3MF / slicer convention; OBJ is
//! rotated into it at parse). Phase 2's orbit camera (#153) starts from this same pose, so the
//! still → live hand-off doesn't jump.

use crate::Bounds;

/// Degrees the camera sits to the right of straight-on-front.
pub(crate) const AZIMUTH_DEG: f32 = 40.0;
/// Degrees the camera sits above the horizon.
pub(crate) const ELEVATION_DEG: f32 = 28.0;
/// Vertical field of view: a mild perspective.
pub(crate) const FOV_Y_DEG: f32 = 30.0;

/// A pinhole camera as an orthonormal basis: `fwd` looks into the scene, `right`/`up` span the
/// image plane.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Camera {
    pub eye: [f32; 3],
    pub right: [f32; 3],
    pub up: [f32; 3],
    pub fwd: [f32; 3],
}

/// Where a camera sits relative to the model: a turntable around `target`. The still uses the
/// home pose; the live view (#153) turns it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Orbit {
    pub target: [f32; 3],
    /// The bounding sphere's radius (floored; see [`Orbit::home`]).
    pub radius: f32,
    /// Radians to the right of straight-on-front (front is −Y).
    pub azimuth: f32,
    /// Radians above the horizon.
    pub elevation: f32,
    pub distance: f32,
}

impl Orbit {
    /// The default pose for `bounds`: a 3/4 view from front-right-above, far enough back that the
    /// whole bounding sphere is in front of the camera (every vertex has positive depth, so no
    /// near-plane clipping is needed).
    pub fn home(bounds: &Bounds) -> Orbit {
        let target = bounds.center();
        let s = bounds.size();
        // Floor the radius relative to the model's distance from the origin: a tiny part far
        // from the origin would otherwise put the eye within an f32 ULP of the centre.
        let magnitude = target.iter().fold(1.0f32, |m, c| m.max(c.abs()));
        let radius = (0.5 * (s[0] * s[0] + s[1] * s[1] + s[2] * s[2]).sqrt()).max(1e-4 * magnitude);
        Orbit {
            target,
            radius,
            azimuth: AZIMUTH_DEG.to_radians(),
            elevation: ELEVATION_DEG.to_radians(),
            distance: radius / (FOV_Y_DEG.to_radians() * 0.5).sin() * 1.05,
        }
    }

    pub fn camera(&self) -> Camera {
        let (az, el) = (self.azimuth, self.elevation);
        // From the target towards the eye: right of front (front is −Y), and up.
        let to_eye = [el.cos() * az.sin(), -el.cos() * az.cos(), el.sin()];
        let eye = [
            self.target[0] + to_eye[0] * self.distance,
            self.target[1] + to_eye[1] * self.distance,
            self.target[2] + to_eye[2] * self.distance,
        ];
        let fwd = scale(to_eye, -1.0);
        let right = normalize(cross(fwd, [0.0, 0.0, 1.0]));
        let up = cross(right, fwd);
        Camera {
            eye,
            right,
            up,
            fwd,
        }
    }
}

impl Camera {
    /// The still's camera for `bounds`: the home orbit pose.
    pub fn framing(bounds: &Bounds) -> Camera {
        Orbit::home(bounds).camera()
    }

    /// Camera-space `(x, y, depth)` of world point `p`; depth is along `fwd`.
    #[inline]
    pub fn view_space(&self, p: [f32; 3]) -> [f32; 3] {
        let d = [p[0] - self.eye[0], p[1] - self.eye[1], p[2] - self.eye[2]];
        [dot(d, self.right), dot(d, self.up), dot(d, self.fwd)]
    }

    /// A direction given in camera terms (right, up, towards-the-camera) as a world vector.
    pub fn world_dir(&self, right: f32, up: f32, back: f32) -> [f32; 3] {
        let v = [
            self.right[0] * right + self.up[0] * up - self.fwd[0] * back,
            self.right[1] * right + self.up[1] * up - self.fwd[1] * back,
            self.right[2] * right + self.up[2] * up - self.fwd[2] * back,
        ];
        normalize(v)
    }
}

#[inline]
pub(crate) fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

#[inline]
pub(crate) fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

#[inline]
pub(crate) fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

#[inline]
fn scale(a: [f32; 3], s: f32) -> [f32; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}

#[inline]
pub(crate) fn normalize(a: [f32; 3]) -> [f32; 3] {
    let len = dot(a, a).sqrt();
    if len > 0.0 {
        scale(a, 1.0 / len)
    } else {
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corners(b: &Bounds) -> Vec<[f32; 3]> {
        (0..8)
            .map(|i| {
                [
                    if i & 1 == 0 { b.min[0] } else { b.max[0] },
                    if i & 2 == 0 { b.min[1] } else { b.max[1] },
                    if i & 4 == 0 { b.min[2] } else { b.max[2] },
                ]
            })
            .collect()
    }

    #[test]
    fn basis_is_orthonormal_and_up_points_up() {
        let cam = Camera::framing(&Bounds {
            min: [0.0; 3],
            max: [1.0; 3],
        });
        for v in [cam.right, cam.up, cam.fwd] {
            assert!((dot(v, v) - 1.0).abs() < 1e-5);
        }
        assert!(dot(cam.right, cam.up).abs() < 1e-5);
        assert!(dot(cam.up, cam.fwd).abs() < 1e-5);
        assert!(cam.up[2] > 0.0, "world +Z reads as up on screen");
        assert!(cam.eye[1] < 0.0, "camera is in front (−Y)");
    }

    proptest::proptest! {
        /// Whatever the box, every corner sits in front of the camera and inside the view cone.
        #[test]
        fn framing_contains_the_whole_box(
            min in proptest::array::uniform3(-1e4f32..1e4),
            ext in proptest::array::uniform3(0f32..1e4),
        ) {
            let b = Bounds { min, max: [min[0] + ext[0], min[1] + ext[1], min[2] + ext[2]] };
            let cam = Camera::framing(&b);
            let half_tan = (FOV_Y_DEG.to_radians() * 0.5).tan();
            for c in corners(&b) {
                let v = cam.view_space(c);
                proptest::prop_assert!(v[2] > 0.0);
                proptest::prop_assert!((v[0] / v[2]).abs() <= half_tan * 1.01 + 1e-4);
                proptest::prop_assert!((v[1] / v[2]).abs() <= half_tan * 1.01 + 1e-4);
            }
        }
    }
}
