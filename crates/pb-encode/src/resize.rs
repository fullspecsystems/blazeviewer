//! **Export resize** (export-plan.md rung 2): downscale-only, Lanczos3, in **linear light**
//! with **premultiplied alpha** — so a fine black-and-white pattern averages to the right grey
//! (not a too-dark one) and a transparent pixel's hidden colour can't bleed into an edge.
//!
//! Linearizing uses the sRGB curve for every profile. That is exact for sRGB and Display P3
//! (which share it) and an approximation for other curves (Adobe RGB's 2.2, ProPhoto's 1.8):
//! the curve is inverted exactly afterward, so flat areas come back unchanged and only the
//! blend weights shift slightly.

use std::sync::OnceLock;

use fast_image_resize::images::Image;
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

use crate::types::{Channels, Samples};

/// How to size an export, **as displayed** (after the RAM rotation). Downscale only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resize {
    /// The longer edge at most this many pixels.
    LongEdge(u32),
    /// Fit inside `width` × `height`, keeping the aspect ratio.
    Fit { width: u32, height: u32 },
}

impl Resize {
    /// The output size for a `w` × `h` image (as displayed), or `None` when it already fits —
    /// an export is never upscaled — or the request is degenerate (a zero edge).
    pub fn target(self, w: u32, h: u32) -> Option<(u32, u32)> {
        if w == 0 || h == 0 {
            return None;
        }
        let scale = match self {
            Resize::LongEdge(edge) => edge as f64 / w.max(h) as f64,
            Resize::Fit { width, height } => {
                (width as f64 / w as f64).min(height as f64 / h as f64)
            }
        };
        if scale <= 0.0 || scale >= 1.0 {
            return None;
        }
        let dim = |d: u32| ((d as f64 * scale).round() as u32).max(1);
        Some((dim(w), dim(h)))
    }
}

fn srgb_to_linear(e: f32) -> f32 {
    if e <= 0.040_45 {
        e / 12.92
    } else {
        ((e + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(l: f32) -> f32 {
    let l = l.clamp(0.0, 1.0);
    if l <= 0.003_130_8 {
        12.92 * l
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

/// 16-bit code → linear, built once (256 KB): a lookup per sample, not a `powf`.
fn lut16() -> &'static [f32] {
    static LUT: OnceLock<Vec<f32>> = OnceLock::new();
    LUT.get_or_init(|| {
        (0..=u16::MAX)
            .map(|v| srgb_to_linear(v as f32 / 65535.0))
            .collect()
    })
}

/// Downscale interleaved encoded `samples` (`w` × `h`, `channels`) to `tw` × `th`. The sample
/// type is kept. Floats are treated as encoded [0, 1].
pub fn resize(samples: &Samples, w: u32, h: u32, channels: Channels, tw: u32, th: u32) -> Samples {
    let n = channels.count();
    let colour = if channels.has_alpha() { n - 1 } else { n };
    let lut8: [f32; 256] = std::array::from_fn(|v| srgb_to_linear(v as f32 / 255.0));

    // Encoded → linear, alpha premultiplied.
    let mut src: Vec<f32> = match samples {
        Samples::U8(v) => v
            .iter()
            .enumerate()
            .map(|(i, &s)| {
                if i % n < colour {
                    lut8[s as usize]
                } else {
                    s as f32 / 255.0
                }
            })
            .collect(),
        Samples::U16(v) => {
            let lut = lut16();
            v.iter()
                .enumerate()
                .map(|(i, &s)| {
                    if i % n < colour {
                        lut[s as usize]
                    } else {
                        s as f32 / 65535.0
                    }
                })
                .collect()
        }
        Samples::F32(v) => v
            .iter()
            .enumerate()
            .map(|(i, &s)| {
                if i % n < colour {
                    srgb_to_linear(s.clamp(0.0, 1.0))
                } else {
                    s
                }
            })
            .collect(),
    };
    if colour < n {
        for px in src.chunks_exact_mut(n) {
            let a = px[n - 1];
            px[..colour].iter_mut().for_each(|c| *c *= a);
        }
    }

    let pixel_type = match n {
        1 => PixelType::F32,
        2 => PixelType::F32x2,
        3 => PixelType::F32x3,
        _ => PixelType::F32x4,
    };
    let mut dst = vec![0f32; tw as usize * th as usize * n];
    {
        // Borrow both buffers as bytes — no copies of what can be a gigabyte of floats.
        let src_img = Image::from_slice_u8(w, h, bytemuck::cast_slice_mut(&mut src), pixel_type)
            .expect("source buffer matches its size");
        let mut dst_img =
            Image::from_slice_u8(tw, th, bytemuck::cast_slice_mut(&mut dst), pixel_type)
                .expect("destination buffer matches its size");
        let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3));
        Resizer::new()
            .resize(&src_img, &mut dst_img, &opts)
            .expect("same pixel type, non-empty sizes");
    }
    drop(src);

    // Un-premultiply, re-encode. Lanczos rings slightly past [0, 1]; clamp.
    if colour < n {
        for px in dst.chunks_exact_mut(n) {
            let a = px[n - 1].clamp(0.0, 1.0);
            px[n - 1] = a;
            px[..colour]
                .iter_mut()
                .for_each(|c| *c = if a > 0.0 { *c / a } else { 0.0 });
        }
    }
    let encode = |i: usize, v: f32| {
        if i % n < colour {
            linear_to_srgb(v)
        } else {
            v.clamp(0.0, 1.0)
        }
    };
    match samples {
        Samples::U8(_) => Samples::U8(
            dst.iter()
                .enumerate()
                .map(|(i, &v)| (encode(i, v) * 255.0 + 0.5) as u8)
                .collect(),
        ),
        Samples::U16(_) => Samples::U16(
            dst.iter()
                .enumerate()
                .map(|(i, &v)| (encode(i, v) * 65535.0 + 0.5) as u16)
                .collect(),
        ),
        Samples::F32(_) => {
            Samples::F32(dst.iter().enumerate().map(|(i, &v)| encode(i, v)).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_downscale_only_and_keep_the_aspect() {
        assert_eq!(
            Resize::LongEdge(2000).target(4000, 3000),
            Some((2000, 1500))
        );
        assert_eq!(
            Resize::LongEdge(2000).target(3000, 4000),
            Some((1500, 2000))
        );
        assert_eq!(
            Resize::Fit {
                width: 1000,
                height: 1000
            }
            .target(4000, 2000),
            Some((1000, 500))
        );
        assert_eq!(
            Resize::LongEdge(5000).target(4000, 3000),
            None,
            "never upscale"
        );
        assert_eq!(
            Resize::LongEdge(4000).target(4000, 3000),
            None,
            "already fits"
        );
        assert_eq!(Resize::LongEdge(0).target(4000, 3000), None, "degenerate");
        assert_eq!(
            Resize::LongEdge(10).target(1000, 1),
            Some((10, 1)),
            "never 0 px"
        );
    }

    #[test]
    fn a_flat_colour_survives_exactly() {
        let px = [12u8, 200, 77].repeat(16 * 8);
        let out = resize(&Samples::U8(px), 16, 8, Channels::Rgb, 5, 3);
        assert_eq!(out, Samples::U8([12u8, 200, 77].repeat(15)));
        let px = [1234u16, 60000, 31000, 40000].repeat(9 * 9);
        let out = resize(&Samples::U16(px), 9, 9, Channels::Rgba, 4, 4);
        let Samples::U16(v) = out else { panic!() };
        for p in v.chunks_exact(4) {
            for (got, want) in p.iter().zip([1234u16, 60000, 31000, 40000]) {
                assert!(got.abs_diff(want) <= 2, "{p:?}");
            }
        }
    }

    /// A black/white checkerboard averages to 50 % *light* — sRGB ≈ 188 — not to code 128,
    /// which is what a gamma-space resize gives (visibly too dark).
    #[test]
    fn averaging_happens_in_linear_light() {
        let mut px = Vec::new();
        for y in 0..8 {
            for x in 0..8 {
                px.push(if (x + y) % 2 == 0 { 0u8 } else { 255 });
            }
        }
        let Samples::U8(v) = resize(&Samples::U8(px), 8, 8, Channels::Gray, 2, 2) else {
            panic!()
        };
        for g in v {
            assert!((184..=192).contains(&g), "linear-light grey ≈ 188, got {g}");
        }
    }

    /// Opaque red beside fully transparent *green*: the resized edge stays pure red — the
    /// hidden green can't bleed in (premultiplied resize).
    #[test]
    fn hidden_colour_under_transparency_does_not_bleed() {
        let mut px = Vec::new();
        for _y in 0..4 {
            for x in 0..8 {
                px.extend_from_slice(if x < 4 {
                    &[255u8, 0, 0, 255]
                } else {
                    &[0, 255, 0, 0]
                });
            }
        }
        let Samples::U8(v) = resize(&Samples::U8(px), 8, 4, Channels::Rgba, 4, 2) else {
            panic!()
        };
        for p in v.chunks_exact(4) {
            if p[3] > 0 {
                assert!(
                    p[1] == 0 && p[0] > 250,
                    "no green in a visible pixel: {p:?}"
                );
            }
        }
    }
}
