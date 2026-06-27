//! Fit-to-screen geometry: scale an image to fill the window as much as possible
//! **without cropping** and centered (letter/pillar-boxed). This is the default
//! "no chrome, fit the photo to the screen" behavior.

/// The on-screen placement of an image: integer size and top-left offset within
/// the window, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FitRect {
    pub width: u32,
    pub height: u32,
    pub offset_x: i32,
    pub offset_y: i32,
}

/// Compute the largest centered rectangle with the image's aspect ratio that
/// fits inside `screen_w × screen_h` without cropping. By default we do not
/// upscale past native size; pass `allow_upscale = true` to fill small images to
/// the screen.
pub fn fit_rect(
    img_w: u32,
    img_h: u32,
    screen_w: u32,
    screen_h: u32,
    allow_upscale: bool,
) -> FitRect {
    if img_w == 0 || img_h == 0 || screen_w == 0 || screen_h == 0 {
        return FitRect {
            width: 0,
            height: 0,
            offset_x: 0,
            offset_y: 0,
        };
    }

    // Scale to fit both dimensions (object-fit: contain).
    let sx = screen_w as f64 / img_w as f64;
    let sy = screen_h as f64 / img_h as f64;
    let mut scale = sx.min(sy);
    if !allow_upscale {
        scale = scale.min(1.0);
    }

    let w = (img_w as f64 * scale).round().max(1.0) as u32;
    let h = (img_h as f64 * scale).round().max(1.0) as u32;
    let offset_x = ((screen_w as i64 - w as i64) / 2) as i32;
    let offset_y = ((screen_h as i64 - h as i64) / 2) as i32;

    FitRect {
        width: w,
        height: h,
        offset_x,
        offset_y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn landscape_into_wide_screen_is_height_bound() {
        // 16:9 image into the 7680x3840 (2:1) ultrawide -> limited by height.
        let r = fit_rect(1920, 1080, 7680, 3840, false);
        // native fits (no upscale): stays 1920x1080, centered.
        assert_eq!((r.width, r.height), (1920, 1080));
        assert_eq!(r.offset_x, (7680 - 1920) / 2);
        assert_eq!(r.offset_y, (3840 - 1080) / 2);
    }

    #[test]
    fn upscales_to_fit_when_allowed_preserving_aspect() {
        let r = fit_rect(1000, 500, 7680, 3840, true);
        // width-bound: 7680 / 1000 = 7.68 vs height 3840/500 = 7.68 -> equal, exact 2:1.
        assert_eq!((r.width, r.height), (7680, 3840));
        assert_eq!((r.offset_x, r.offset_y), (0, 0));
    }

    #[test]
    fn portrait_is_width_centered() {
        let r = fit_rect(2000, 4000, 7680, 3840, true);
        // height-bound: scale = 3840/4000 = 0.96 -> 1920x3840, centered horizontally.
        assert_eq!((r.width, r.height), (1920, 3840));
        assert_eq!(r.offset_y, 0);
        assert_eq!(r.offset_x, (7680 - 1920) / 2);
    }

    #[test]
    fn never_crops_aspect_ratio() {
        let r = fit_rect(1234, 567, 800, 600, true);
        let in_aspect = 1234.0 / 567.0;
        let out_aspect = r.width as f64 / r.height as f64;
        assert!((in_aspect - out_aspect).abs() < 0.01);
        assert!(r.width <= 800 && r.height <= 600);
    }

    #[test]
    fn degenerate_inputs_are_empty() {
        assert_eq!(fit_rect(0, 100, 800, 600, true).width, 0);
        assert_eq!(fit_rect(100, 100, 0, 600, true).height, 0);
    }
}
