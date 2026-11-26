//! jpegli, libjxl's JPEG encoder, for Blaze Viewer's JPEG export (task #157): one safe function
//! over a C++ shim (`src/shim.cc`) that runs a whole encode, so no libjpeg struct crosses into
//! Rust and no Rust panic unwinds through C++.
//!
//! At equal SSIMULACRA2, jpegli writes files 8–10 % smaller than the `image` crate's baseline
//! encoder across q70–q90 and faster (export-plan.md rung 4). It is built from vendored sources
//! against jxl-sys's Highway: see `build.rs` and `vendor/README.md`.

use std::ffi::{c_char, c_int, c_void};

// jpegli's objects call into Highway; jxl-sys links the one libhwy they resolve against.
use jxl_sys as _;

/// The longest APP1 payload a JPEG marker holds (65,535 less the two length bytes).
const MAX_APP1: usize = 65_533;
/// The longest profile: 255 `ICC_PROFILE` chunks of 65,519 bytes (the sequence byte is a u8).
const MAX_ICC: usize = 255 * 65_519;

/// Why an encode failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// One image to encode: 8-bit samples, rows packed with no padding.
#[derive(Debug, Clone, Copy)]
pub struct Image<'a> {
    pub width: u32,
    pub height: u32,
    /// 1 (gray) or 3 (RGB).
    pub components: u32,
    pub pixels: &'a [u8],
}

/// What goes into the file besides the pixels.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options<'a> {
    /// libjpeg quality, 1–100 (clamped).
    pub quality: u8,
    /// An EXIF block from its TIFF header on (no `Exif\0\0`), written as `APP1`.
    pub exif: Option<&'a [u8]>,
    /// An ICC profile, written as numbered `APP2` `ICC_PROFILE` chunks (from 1, as ICC.1 says).
    pub icc: Option<&'a [u8]>,
}

extern "C" {
    #[allow(clippy::too_many_arguments)]
    fn pb_jpegli_encode(
        pixels: *const u8,
        width: u32,
        height: u32,
        components: u32,
        stride: usize,
        quality: c_int,
        app1: *const u8,
        app1_len: usize,
        icc: *const u8,
        icc_len: usize,
        sink: extern "C" fn(*mut c_void, *const u8, usize),
        ctx: *mut c_void,
        msg: *mut c_char,
        msg_cap: usize,
    ) -> c_int;
}

/// The shim's one call back with the finished file.
extern "C" fn sink(ctx: *mut c_void, bytes: *const u8, len: usize) {
    if len == 0 {
        return;
    }
    // SAFETY: `ctx` is the `&mut Vec<u8>` `encode` passed, alive for the whole call; `bytes`
    // points at `len` initialized bytes the shim owns until this returns.
    let (out, bytes) = unsafe {
        (
            &mut *ctx.cast::<Vec<u8>>(),
            std::slice::from_raw_parts(bytes, len),
        )
    };
    out.extend_from_slice(bytes);
}

/// Encode `image` as a progressive, 4:4:4 JPEG at `options.quality`.
pub fn encode(image: Image<'_>, options: Options<'_>) -> Result<Vec<u8>, Error> {
    let fail = |m: &str| Err(Error(m.to_owned()));
    if !matches!(image.components, 1 | 3) {
        return fail("jpegli: 1 or 3 components only");
    }
    let stride = (image.width as usize)
        .checked_mul(image.components as usize)
        .ok_or_else(|| Error("jpegli: image too large".into()))?;
    let expected = stride.checked_mul(image.height as usize);
    if image.width == 0 || image.height == 0 || expected != Some(image.pixels.len()) {
        return fail("jpegli: pixel count doesn't match the dimensions");
    }
    let app1 = match options.exif {
        Some(exif) if !exif.is_empty() => {
            if exif.len() + 6 > MAX_APP1 {
                return fail("jpegli: EXIF block too large for one APP1 segment");
            }
            let mut v = Vec::with_capacity(exif.len() + 6);
            v.extend_from_slice(b"Exif\0\0");
            v.extend_from_slice(exif);
            v
        }
        _ => Vec::new(),
    };
    let icc = options.icc.unwrap_or_default();
    if icc.len() > MAX_ICC {
        return fail("jpegli: colour profile too large for a JPEG");
    }

    let mut out: Vec<u8> = Vec::new();
    let mut msg = [0 as c_char; 256];
    // SAFETY: every pointer is valid for the length passed beside it for the whole call
    // (`pixels` holds `stride * height` bytes, checked above); `sink` only writes through
    // `ctx` = `&mut out`; the shim writes at most `msg.len()` bytes, NUL-terminated.
    let status = unsafe {
        pb_jpegli_encode(
            image.pixels.as_ptr(),
            image.width,
            image.height,
            image.components,
            stride,
            c_int::from(options.quality.clamp(1, 100)),
            app1.as_ptr(),
            app1.len(),
            icc.as_ptr(),
            icc.len(),
            sink,
            (&mut out as *mut Vec<u8>).cast(),
            msg.as_mut_ptr(),
            msg.len(),
        )
    };
    if status == 0 {
        Ok(out)
    } else {
        // SAFETY: the shim NUL-terminates within the buffer on every failure path.
        let text = unsafe { std::ffi::CStr::from_ptr(msg.as_ptr()) }.to_string_lossy();
        Err(Error(format!("jpegli: {text}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageDecoder as _;

    fn gradient(w: u32, h: u32) -> Vec<u8> {
        (0..h)
            .flat_map(|y| (0..w).flat_map(move |x| [(x * 255 / w) as u8, (y * 255 / h) as u8, 90]))
            .collect()
    }

    fn rgb(w: u32, h: u32, px: &[u8]) -> Image<'_> {
        Image {
            width: w,
            height: h,
            components: 3,
            pixels: px,
        }
    }

    fn opts(quality: u8) -> Options<'static> {
        Options {
            quality,
            ..Default::default()
        }
    }

    /// The `(marker, payload)` segments before the first scan.
    fn segments(jpeg: &[u8]) -> Vec<(u8, &[u8])> {
        let mut out = Vec::new();
        let mut i = 2;
        while i + 4 <= jpeg.len() && jpeg[i] == 0xFF && jpeg[i + 1] != 0xDA {
            let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
            out.push((jpeg[i + 1], &jpeg[i + 4..i + 2 + len]));
            i += 2 + len;
        }
        out
    }

    #[test]
    fn rgb_round_trips_close_to_the_source() {
        let (w, h) = (64, 48);
        let px = gradient(w, h);
        let jpeg = encode(rgb(w, h, &px), opts(90)).unwrap();
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
        assert_eq!(&jpeg[jpeg.len() - 2..], &[0xFF, 0xD9]);
        let back = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
            .unwrap()
            .to_rgb8();
        assert_eq!(back.dimensions(), (w, h));
        let worst = px
            .iter()
            .zip(back.as_raw())
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        assert!(worst <= 12, "max channel error {worst}");
    }

    #[test]
    fn gray_stays_gray() {
        let px: Vec<u8> = (0..32 * 32).map(|i| (i % 251) as u8).collect();
        let jpeg = encode(
            Image {
                width: 32,
                height: 32,
                components: 1,
                pixels: &px,
            },
            opts(85),
        )
        .unwrap();
        let dec = image::codecs::jpeg::JpegDecoder::new(std::io::Cursor::new(&jpeg)).unwrap();
        assert_eq!(dec.color_type(), image::ColorType::L8);
    }

    #[test]
    fn it_is_progressive_and_444() {
        let px = gradient(16, 16);
        let jpeg = encode(rgb(16, 16, &px), opts(85)).unwrap();
        let segs = segments(&jpeg);
        let (_, sof) = *segs
            .iter()
            .find(|(m, _)| (0xC0..=0xC2).contains(m))
            .unwrap();
        assert!(segs.iter().any(|(m, _)| *m == 0xC2), "SOF2 (progressive)");
        // SOF: precision, height, width, components, then (id, sampling, table) per component.
        assert_eq!(sof[5], 3);
        for c in 0..3 {
            assert_eq!(sof[6 + c * 3 + 1], 0x11, "component {c} is 1x1 (4:4:4)");
        }
    }

    /// A profile longer than one marker splits into chunks numbered from 1 (the `mozjpeg` crate
    /// numbered from 0, and decoders dropped the profile), and reads back whole.
    #[test]
    fn a_long_profile_splits_into_chunks_numbered_from_one() {
        let icc: Vec<u8> = (0..150_000u32).map(|i| (i * 7 % 256) as u8).collect();
        let px = gradient(8, 8);
        let jpeg = encode(
            rgb(8, 8, &px),
            Options {
                quality: 80,
                icc: Some(&icc),
                ..Default::default()
            },
        )
        .unwrap();
        let chunks: Vec<(u8, u8)> = segments(&jpeg)
            .into_iter()
            .filter(|(m, p)| *m == 0xE2 && p.starts_with(b"ICC_PROFILE\0"))
            .map(|(_, p)| (p[12], p[13]))
            .collect();
        assert_eq!(chunks, vec![(1, 3), (2, 3), (3, 3)]);
        let mut dec = image::codecs::jpeg::JpegDecoder::new(std::io::Cursor::new(&jpeg)).unwrap();
        assert_eq!(dec.icc_profile().unwrap(), Some(icc));
    }

    #[test]
    fn exif_goes_in_app1_with_its_header() {
        let exif = b"MM\0*\0\0\0\x08\0\0".to_vec();
        let px = gradient(8, 8);
        let jpeg = encode(
            rgb(8, 8, &px),
            Options {
                quality: 80,
                exif: Some(&exif),
                ..Default::default()
            },
        )
        .unwrap();
        let app1: Vec<&[u8]> = segments(&jpeg)
            .into_iter()
            .filter(|(m, _)| *m == 0xE1)
            .map(|(_, p)| p)
            .collect();
        assert_eq!(
            app1,
            vec![[b"Exif\0\0".as_slice(), &exif].concat().as_slice()]
        );
        let plain = encode(rgb(8, 8, &px), opts(80)).unwrap();
        assert!(!segments(&plain).iter().any(|(m, _)| *m == 0xE1));
    }

    #[test]
    fn bad_input_is_refused_in_rust() {
        let px = gradient(4, 4);
        assert!(encode(rgb(4, 5, &px), opts(80)).is_err());
        assert!(encode(rgb(0, 0, &[]), opts(80)).is_err());
        let two = Image {
            components: 2,
            ..rgb(4, 4, &px)
        };
        assert!(encode(two, opts(80)).is_err());
        let huge = vec![0u8; MAX_APP1];
        let with_huge = Options {
            exif: Some(&huge),
            ..opts(80)
        };
        assert!(encode(rgb(4, 4, &px), with_huge).is_err());
    }

    /// jpegli's own errors come back as `Err` with its message (the shim's throwing
    /// `error_exit`), not as jpegli's default `exit()`.
    #[test]
    fn a_jpegli_error_is_an_err_not_an_exit() {
        let px = vec![0u8; 70_000];
        let err = encode(
            Image {
                width: 70_000,
                height: 1,
                components: 1,
                pixels: &px,
            },
            opts(80),
        )
        .unwrap_err();
        assert!(err.0.starts_with("jpegli: "), "{err}");
        assert!(err.0.len() > "jpegli: ".len(), "{err}");
        // And the encoder still works afterwards.
        let ok = gradient(8, 8);
        assert!(encode(rgb(8, 8, &ok), opts(80)).is_ok());
    }

    #[test]
    fn encodes_run_side_by_side() {
        let px = gradient(256, 256);
        let one = encode(rgb(256, 256, &px), opts(85)).unwrap();
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| s.spawn(|| encode(rgb(256, 256, &px), opts(85)).unwrap()))
                .collect();
            for h in handles {
                assert_eq!(h.join().unwrap(), one, "deterministic across threads");
            }
        });
    }
}
