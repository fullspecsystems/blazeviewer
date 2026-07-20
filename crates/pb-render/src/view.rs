//! The per-photo **view transform** — the shared geometry behind scaling modes
//! (tasks.json #4), rotation (#1), zoom and pan (#3).
//!
//! It composes a base scaling mode (Fit / Fill / Original) with a 90° rotation
//! quadrant, a zoom multiplier, and a pan offset into the final on-screen
//! placement of the image plus the texture UVs to sample. Pure and unit-tested —
//! the GPU just turns the resulting rect + UVs into a quad, so rotation/zoom/pan
//! are perf-neutral transforms with no re-decode.

use crate::ScaleMode;

/// 90-degree rotation steps, clockwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rotation {
    #[default]
    R0,
    R90,
    R180,
    R270,
}

impl Rotation {
    pub fn cw(self) -> Self {
        match self {
            Rotation::R0 => Rotation::R90,
            Rotation::R90 => Rotation::R180,
            Rotation::R180 => Rotation::R270,
            Rotation::R270 => Rotation::R0,
        }
    }

    pub fn ccw(self) -> Self {
        match self {
            Rotation::R0 => Rotation::R270,
            Rotation::R90 => Rotation::R0,
            Rotation::R180 => Rotation::R90,
            Rotation::R270 => Rotation::R180,
        }
    }

    /// True for 90°/270°, where the displayed width/height swap.
    pub fn swaps_axes(self) -> bool {
        matches!(self, Rotation::R90 | Rotation::R270)
    }

    /// Per-corner texture UVs in quad order (top-left, top-right, bottom-right,
    /// bottom-left) so the sampled image appears rotated by this quadrant.
    fn corner_uvs(self) -> [[f32; 2]; 4] {
        match self {
            Rotation::R0 => [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            Rotation::R90 => [[0.0, 1.0], [0.0, 0.0], [1.0, 0.0], [1.0, 1.0]],
            Rotation::R180 => [[1.0, 1.0], [0.0, 1.0], [0.0, 0.0], [1.0, 0.0]],
            Rotation::R270 => [[1.0, 0.0], [1.0, 1.0], [0.0, 1.0], [0.0, 0.0]],
        }
    }
}

/// Zoom clamp bounds (multipliers on the base scale).
pub const MIN_ZOOM: f32 = 0.05;
pub const MAX_ZOOM: f32 = 32.0;

/// A per-photo transform: base scaling mode + rotation + zoom + pan.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewTransform {
    pub mode: ScaleMode,
    pub rotation: Rotation,
    /// Multiplier on the mode's base scale; clamped to `[MIN_ZOOM, MAX_ZOOM]`.
    pub zoom: f32,
    /// Pan offset from centered, in screen pixels (clamped to image bounds).
    pub pan: [f32; 2],
}

impl Default for ViewTransform {
    fn default() -> Self {
        Self {
            mode: ScaleMode::Fit,
            rotation: Rotation::R0,
            zoom: 1.0,
            pan: [0.0, 0.0],
        }
    }
}

/// Screen-space placement of the image: a float rect (origin may be negative or
/// overflow the screen) plus the per-corner UVs (TL, TR, BR, BL) to sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub uvs: [[f32; 2]; 4],
}

impl ViewTransform {
    /// The rotated image dimensions (width/height swapped for 90°/270°).
    fn rotated_dims(&self, img_w: u32, img_h: u32) -> (f32, f32) {
        if self.rotation.swaps_axes() {
            (img_h as f32, img_w as f32)
        } else {
            (img_w as f32, img_h as f32)
        }
    }

    /// The base (zoom = 1) scale for the current mode.
    fn base_scale(&self, rw: f32, rh: f32, sw: f32, sh: f32) -> f32 {
        match self.mode {
            ScaleMode::Fit => (sw / rw).min(sh / rh),
            ScaleMode::Fill => (sw / rw).max(sh / rh),
            ScaleMode::Original => 1.0,
        }
    }

    fn displayed_size(&self, img_w: u32, img_h: u32, screen_w: u32, screen_h: u32) -> (f32, f32) {
        let (rw, rh) = self.rotated_dims(img_w, img_h);
        if rw <= 0.0 || rh <= 0.0 || screen_w == 0 || screen_h == 0 {
            return (0.0, 0.0);
        }
        let s = self.base_scale(rw, rh, screen_w as f32, screen_h as f32)
            * self.zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        (rw * s, rh * s)
    }

    /// Overflow smaller than this (physical px, per side) is fit/zoom **rounding**,
    /// not real pan headroom. A width-constrained image in Fit computes
    /// `displayed_w = rw * (screen_w / rw)`, which in f32 can land a hair over
    /// `screen_w` (e.g. 2560.0002), and an imperceptible zoom (1.000…) leaves the
    /// same sub-pixel sliver. Below this threshold the image is treated as fitting,
    /// so a video's horizontal arrows **seek** instead of panning invisibly
    /// (owner-reported: an unnoticeable overflow silently killed arrow-seek).
    pub const PAN_DEADZONE_PX: f32 = 1.0;

    /// Whether there is *meaningful* pan headroom on each axis — more than the
    /// [`Self::PAN_DEADZONE_PX`] rounding deadzone. Drives the seek-vs-pan choice
    /// for a video's arrow keys and the grab-hand cursor affordance; the pan
    /// *clamp* still uses the raw [`Self::max_pan`], so a real sub-pixel bound is
    /// respected even though it doesn't flip the affordance.
    pub fn pannable_axes(&self, img_w: u32, img_h: u32, screen_w: u32, screen_h: u32) -> [bool; 2] {
        let mp = self.max_pan(img_w, img_h, screen_w, screen_h);
        [mp[0] > Self::PAN_DEADZONE_PX, mp[1] > Self::PAN_DEADZONE_PX]
    }

    /// Largest pan offset (px, each axis) that keeps the image covering the
    /// viewport — zero when the image is ≤ the screen on that axis (so it stays
    /// centered and pan is a no-op).
    pub fn max_pan(&self, img_w: u32, img_h: u32, screen_w: u32, screen_h: u32) -> [f32; 2] {
        let (dw, dh) = self.displayed_size(img_w, img_h, screen_w, screen_h);
        [
            ((dw - screen_w as f32) / 2.0).max(0.0),
            ((dh - screen_h as f32) / 2.0).max(0.0),
        ]
    }

    /// Multiply the zoom by `factor` (clamped to `[MIN_ZOOM, MAX_ZOOM]`) while
    /// keeping the image point currently under the screen-space `anchor` pinned
    /// to that anchor — the "pinch/scroll toward the pointer" behavior. Adjusts
    /// `pan` to compensate, then re-clamps it to the image bounds. A no-op if the
    /// image/screen is degenerate or the zoom is already at the requested bound.
    ///
    /// `anchor` is in screen pixels (same space as `pan`). When the image is
    /// smaller than the screen on an axis, `max_pan` is zero there, so it simply
    /// zooms about the screen center on that axis (there is nothing to pan to).
    pub fn zoom_about(
        &mut self,
        factor: f32,
        anchor: [f32; 2],
        img_w: u32,
        img_h: u32,
        screen_w: u32,
        screen_h: u32,
    ) {
        // Fraction of the *displayed* image under the anchor before the zoom.
        // Use the real placement so any existing pan-clamp is accounted for.
        let before = self.placement(img_w, img_h, screen_w, screen_h);
        if before.w <= 0.0 || before.h <= 0.0 {
            return;
        }
        let u = (anchor[0] - before.x) / before.w;
        let v = (anchor[1] - before.y) / before.h;

        let new_zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        if new_zoom == self.zoom {
            return; // already pinned at a clamp bound — nothing to re-anchor
        }
        self.zoom = new_zoom;

        // Choose pan so the same (u, v) lands back under the anchor:
        // anchor = top_left + (u, v) * new_size, and pan = center − screen_center.
        let (dw, dh) = self.displayed_size(img_w, img_h, screen_w, screen_h);
        let new_x = anchor[0] - u * dw;
        let new_y = anchor[1] - v * dh;
        self.pan = [
            new_x + dw / 2.0 - screen_w as f32 / 2.0,
            new_y + dh / 2.0 - screen_h as f32 / 2.0,
        ];
        let mp = self.max_pan(img_w, img_h, screen_w, screen_h);
        self.pan[0] = self.pan[0].clamp(-mp[0], mp[0]);
        self.pan[1] = self.pan[1].clamp(-mp[1], mp[1]);
    }

    /// Compute the on-screen placement + UVs. Pan is clamped to the image bounds.
    pub fn placement(&self, img_w: u32, img_h: u32, screen_w: u32, screen_h: u32) -> Placement {
        let (sw, sh) = (screen_w as f32, screen_h as f32);
        let (dw, dh) = self.displayed_size(img_w, img_h, screen_w, screen_h);
        let mp = self.max_pan(img_w, img_h, screen_w, screen_h);
        let cx = sw / 2.0 + self.pan[0].clamp(-mp[0], mp[0]);
        let cy = sh / 2.0 + self.pan[1].clamp(-mp[1], mp[1]);
        Placement {
            x: cx - dw / 2.0,
            y: cy - dh / 2.0,
            w: dw,
            h: dh,
            uvs: self.rotation.corner_uvs(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn rotation_steps_cycle() {
        let mut r = Rotation::R0;
        for _ in 0..4 {
            r = r.cw();
        }
        assert_eq!(r, Rotation::R0);
        assert_eq!(Rotation::R0.cw(), Rotation::R90);
        assert_eq!(Rotation::R0.ccw(), Rotation::R270);
        assert_eq!(Rotation::R90.ccw(), Rotation::R0);
        assert!(Rotation::R90.swaps_axes() && Rotation::R270.swaps_axes());
        assert!(!Rotation::R0.swaps_axes() && !Rotation::R180.swaps_axes());
    }

    #[test]
    fn fit_contains_without_cropping() {
        let v = ViewTransform::default(); // Fit, no rotation/zoom/pan
        let p = v.placement(1000, 500, 800, 800);
        // 2:1 image into 800x800 -> width-bound, 800x400, centered.
        assert!(approx(p.w, 800.0) && approx(p.h, 400.0), "{p:?}");
        assert!(approx(p.x, 0.0) && approx(p.y, 200.0), "{p:?}");
        assert_eq!(p.uvs, Rotation::R0.corner_uvs());
    }

    #[test]
    fn fill_covers_and_overflows() {
        let v = ViewTransform {
            mode: ScaleMode::Fill,
            ..Default::default()
        };
        let p = v.placement(1000, 500, 800, 800);
        // Cover: scale = max(0.8, 1.6) = 1.6 -> 1600x800. Overflows width.
        assert!(p.w >= 800.0 && p.h >= 800.0, "must cover: {p:?}");
        assert!(p.x < 0.0, "wide overflow hangs off-screen: {p:?}");
    }

    #[test]
    fn original_is_native_times_zoom() {
        let v = ViewTransform {
            mode: ScaleMode::Original,
            zoom: 2.0,
            ..Default::default()
        };
        let p = v.placement(100, 50, 800, 800);
        assert!(approx(p.w, 200.0) && approx(p.h, 100.0), "{p:?}");
    }

    #[test]
    fn rotation_swaps_displayed_aspect() {
        // A landscape image rotated 90° fits as a portrait.
        let v = ViewTransform {
            rotation: Rotation::R90,
            ..Default::default()
        };
        let p = v.placement(1000, 500, 800, 800);
        // Rotated dims 500x1000 (2:1 portrait) into 800x800 -> height-bound 400x800.
        assert!(approx(p.w, 400.0) && approx(p.h, 800.0), "{p:?}");
        assert_eq!(p.uvs, Rotation::R90.corner_uvs());
    }

    #[test]
    fn pan_is_clamped_to_image_bounds() {
        // Zoomed 2x past fit so the image overflows and pan is meaningful.
        let v = ViewTransform {
            zoom: 2.0,
            pan: [100000.0, 0.0], // absurd pan
            ..Default::default()
        };
        let (iw, ih, sw, sh) = (800, 800, 800, 800);
        let mp = v.max_pan(iw, ih, sw, sh);
        assert!(
            approx(mp[0], 400.0),
            "zoomed 2x: max pan = (1600-800)/2: {mp:?}"
        );
        let p = v.placement(iw, ih, sw, sh);
        // Clamped to +400: center at 400+400=800, so the 1600-wide image's left
        // edge sits at 0 and it still covers the screen (right edge at 1600).
        assert!(
            approx(p.x, 0.0),
            "pan clamped, left edge at screen edge: {p:?}"
        );
        assert!(p.x <= 0.0 && p.x + p.w >= sw as f32, "still covers: {p:?}");
    }

    #[test]
    fn small_image_cannot_pan() {
        // Fit makes the image fill at most the screen, so there's nothing to pan.
        let v = ViewTransform::default();
        let mp = v.max_pan(400, 300, 800, 800);
        assert_eq!(mp, [0.0, 0.0]);
    }

    #[test]
    fn zoom_about_keeps_anchor_point_fixed() {
        // A large image that overflows the screen on both axes (Original mode),
        // so pan is free and the anchored point can be held exactly.
        let mut v = ViewTransform {
            mode: ScaleMode::Original,
            ..Default::default()
        };
        let (iw, ih, sw, sh) = (2000, 2000, 800, 800);
        let anchor = [600.0, 300.0];

        let p0 = v.placement(iw, ih, sw, sh);
        let (u0, w0) = ((anchor[0] - p0.x) / p0.w, (anchor[1] - p0.y) / p0.h);

        v.zoom_about(2.0, anchor, iw, ih, sw, sh);
        assert!(approx(v.zoom, 2.0), "zoom applied: {}", v.zoom);

        let p1 = v.placement(iw, ih, sw, sh);
        let (u1, w1) = ((anchor[0] - p1.x) / p1.w, (anchor[1] - p1.y) / p1.h);
        assert!(
            approx(u0, u1) && approx(w0, w1),
            "anchor drifted: ({u0},{w0}) -> ({u1},{w1})"
        );
    }

    #[test]
    fn zoom_about_center_stays_centered() {
        let mut v = ViewTransform {
            mode: ScaleMode::Original,
            ..Default::default()
        };
        let (iw, ih, sw, sh) = (2000, 2000, 800, 800);
        v.zoom_about(2.0, [400.0, 400.0], iw, ih, sw, sh); // exact screen center
        assert!(
            approx(v.pan[0], 0.0) && approx(v.pan[1], 0.0),
            "centered zoom should not pan: {:?}",
            v.pan
        );
    }

    #[test]
    fn zoom_about_corner_respects_pan_clamp() {
        // Fit a square image to a square screen (fills exactly), then zoom 4x
        // about the top-left corner. The resulting pan must stay within bounds
        // and the image must still cover the screen (no letterbox gap).
        let mut v = ViewTransform::default();
        let (iw, ih, sw, sh) = (1000, 1000, 800, 800);
        v.zoom_about(4.0, [0.0, 0.0], iw, ih, sw, sh);
        let mp = v.max_pan(iw, ih, sw, sh);
        assert!(
            v.pan[0].abs() <= mp[0] + 1e-3 && v.pan[1].abs() <= mp[1] + 1e-3,
            "pan {:?} exceeds max {mp:?}",
            v.pan
        );
        let p = v.placement(iw, ih, sw, sh);
        assert!(
            p.x <= 0.0 && p.x + p.w >= sw as f32 && p.y <= 0.0 && p.y + p.h >= sh as f32,
            "must still cover the screen: {p:?}"
        );
    }

    #[test]
    fn zoom_about_is_clamped_to_max() {
        let mut v = ViewTransform {
            mode: ScaleMode::Original,
            ..Default::default()
        };
        v.zoom_about(1000.0, [400.0, 400.0], 100, 100, 800, 800);
        assert!(approx(v.zoom, MAX_ZOOM), "clamped to MAX: {}", v.zoom);
    }

    #[test]
    fn zoom_is_clamped() {
        let v = ViewTransform {
            mode: ScaleMode::Original,
            zoom: 1000.0, // beyond MAX_ZOOM
            ..Default::default()
        };
        let p = v.placement(100, 100, 800, 800);
        assert!(approx(p.w, 100.0 * MAX_ZOOM), "zoom clamped to MAX: {p:?}");
    }

    #[test]
    fn sub_pixel_fit_overflow_is_not_pannable() {
        // A width-constrained 16:9 image fit into a 16:9 area: fit scale =
        // screen_w / img_w, and img_w * (screen_w / img_w) can land a hair over
        // screen_w in f32. That rounding sliver must NOT read as pan headroom —
        // else a video's horizontal arrows pan invisibly instead of seeking
        // (owner-reported: Fit mode "looked okay" but arrows did nothing).
        let v = ViewTransform::default(); // Fit, zoom 1.0
        let mp = v.max_pan(1920, 1080, 2560, 1440);
        assert!(
            mp[0] < ViewTransform::PAN_DEADZONE_PX,
            "fit overflow {} must be within the deadzone",
            mp[0]
        );
        assert!(
            !v.pannable_axes(1920, 1080, 2560, 1440)[0],
            "a width-fit video is not horizontally pannable"
        );
    }

    #[test]
    fn imperceptible_zoom_is_not_pannable_but_a_real_one_is() {
        // A width-filling video (displayed_w == screen_w at zoom 1), with ~0.25 px of
        // overflow per side — an unnoticeable zoom → still seek.
        let mut v = ViewTransform {
            zoom: 1.0 + 0.5 / 2560.0,
            ..Default::default()
        };
        assert!(
            !v.pannable_axes(2560, 1440, 2560, 1440)[0],
            "a sub-pixel zoom must not flip arrows into pan"
        );
        // A deliberate zoom with several px of headroom → genuinely pannable.
        v.zoom = 1.0 + 8.0 / 2560.0;
        assert!(
            v.pannable_axes(2560, 1440, 2560, 1440)[0],
            "a real zoom is horizontally pannable"
        );
    }

    /// The **no-jump invariant** behind task #124: swapping the bound texture between the
    /// fit-sized `Fit` rep and the full-res `Original` rep must be geometrically invisible.
    ///
    /// It holds because `base_scale` is computed from *the bound texture's own dims*, so the
    /// Fit rep's `1/k` exactly cancels the decode-to-fit scale `k`:
    ///   Fit:      dims (ow*k, oh*k), base = min(sw/(ow*k), sh/(oh*k)) = 1/k  -> displayed = ow*k*zoom
    ///   Original: dims (ow, oh),     base = min(sw/ow, sh/oh)         = k    -> displayed = ow*k*zoom
    ///
    /// This is the claim the whole #124 rebind rests on: if it were false, zooming would visibly
    /// snap the moment the representation changed, and a geometry stash (#123) would be needed.
    #[test]
    fn fit_and_original_reps_display_at_the_same_size() {
        // (orig_w, orig_h, screen_w, screen_h) — landscape, portrait, square, extreme aspect,
        // and a source SMALLER than the screen (k clamps to 1, both reps identical).
        let cases = [
            (6000u32, 4000u32, 3840u32, 2160u32),
            (4000, 6000, 3840, 2160),
            (5000, 5000, 2560, 1440),
            (12000, 2000, 3840, 2160),
            (8256, 5504, 7680, 2160),
            (800, 600, 3840, 2160), // k == 1.0: no downscale
        ];
        for (ow, oh, sw, sh) in cases {
            // The decoder's scale, and the integer dims it rounds to (pb-decode `downscale_to_fit`).
            let k = (sw as f64 / ow as f64).min(sh as f64 / oh as f64).min(1.0);
            let fw = ((ow as f64 * k).round() as u32).max(1);
            let fh = ((oh as f64 * k).round() as u32).max(1);
            for rotation in [Rotation::R0, Rotation::R90, Rotation::R180, Rotation::R270] {
                for zoom in [0.5f32, 1.0, 1.5, 3.0, 8.0, MAX_ZOOM] {
                    let v = ViewTransform {
                        mode: ScaleMode::Fit,
                        rotation,
                        zoom,
                        pan: [0.0, 0.0],
                    };
                    let (fit_w, fit_h) = v.displayed_size(fw, fh, sw, sh);
                    let (org_w, org_h) = v.displayed_size(ow, oh, sw, sh);
                    // Sub-pixel rounding on the non-constraining axis is the only permitted
                    // difference, and it must stay under the pan deadzone so it can never flip
                    // a pan affordance. Scale the tolerance with zoom: a rounding error in the
                    // texture is magnified by the zoom factor, which is expected and harmless.
                    let tol = ViewTransform::PAN_DEADZONE_PX * zoom.max(1.0);
                    assert!(
                        (fit_w - org_w).abs() <= tol && (fit_h - org_h).abs() <= tol,
                        "rep swap moved the image: {ow}x{oh} @ {sw}x{sh} {rotation:?} zoom {zoom}                          -> fit {fit_w}x{fit_h} vs original {org_w}x{org_h} (tol {tol})"
                    );
                }
            }
        }
    }
}
