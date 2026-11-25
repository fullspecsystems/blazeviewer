//! **JPEG XL** over libjxl 0.12 (export-plan.md rung 4), vendored and built by `jxl-sys`
//! (MIT OR Apache-2.0; libjxl BSD-3 with brotli, highway and skcms — no lcms). Never
//! `jpegxl-rs` / `jpegxl-sys`: they are GPL.
//!
//! Lossy (libjxl's quality → Butteraugli distance mapping, XYB inside) or lossless (exact);
//! 8- or 16-bit, gray or RGB, with alpha. The colour goes in as libjxl's sRGB encoding or the
//! ICC profile itself, so a lossy file decodes back to the original space.
//!
//! The encoder and its thread pool live only inside one call, freed by [`Session`]'s `Drop`
//! whatever path leaves.

use std::ffi::c_void;

use jxl_sys as j;

use crate::encoder::{Caps, Encoder};
use crate::types::{ExportImage, OutputColor, Samples};
use crate::EncodeError;

/// libjxl's effort (1 fastest – 10 slowest; 7 is its default). Chosen from corpus timings
/// (export-plan.md rung 4) — the live preview re-encodes on every settings change.
pub const EFFORT: i64 = 7;

/// The JPEG XL export format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JxlEncoder {
    /// 1–100 (90 ≈ distance 1.0, "visually lossless"); ignored when `lossless`.
    pub quality: u8,
    pub lossless: bool,
}

impl Encoder for JxlEncoder {
    fn caps(&self) -> Caps {
        Caps::ALL
    }
    fn extension(&self) -> &'static str {
        "jxl"
    }
    fn lossless(&self) -> bool {
        self.lossless
    }
    fn encode(&self, img: &ExportImage) -> Result<Vec<u8>, EncodeError> {
        encode_jxl(img, self.quality, self.lossless)
    }
}

fn codec(why: impl std::fmt::Display) -> EncodeError {
    EncodeError::Codec(format!("jpeg xl: {why}"))
}

fn ok(status: j::JxlEncoderStatus, what: &str) -> Result<(), EncodeError> {
    if status == j::JxlEncoderStatus::JXL_ENC_SUCCESS {
        Ok(())
    } else {
        Err(codec(what))
    }
}

/// The encoder and its thread pool, destroyed together.
struct Session {
    enc: *mut j::JxlEncoder,
    runner: *mut c_void,
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            if !self.enc.is_null() {
                j::JxlEncoderDestroy(self.enc);
            }
            if !self.runner.is_null() {
                j::JxlThreadParallelRunnerDestroy(self.runner);
            }
        }
    }
}

/// Half the machine, as for AVIF: the export runs at background priority beside browsing.
fn threads() -> usize {
    std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).max(1))
}

/// Encode a prepared image as JPEG XL.
pub fn encode_jxl(img: &ExportImage, quality: u8, lossless: bool) -> Result<Vec<u8>, EncodeError> {
    encode_jxl_at(img, quality, lossless, EFFORT)
}

/// [`encode_jxl`] at another libjxl `effort` (1–10) — for measuring, not the app.
pub fn encode_jxl_at(
    img: &ExportImage,
    quality: u8,
    lossless: bool,
    effort: i64,
) -> Result<Vec<u8>, EncodeError> {
    crate::px::check(img)?;
    let (data_type, bits, bytes): (j::JxlDataType, u32, &[u8]) = match &img.samples {
        Samples::U8(v) => (j::JxlDataType::JXL_TYPE_UINT8, 8, v.as_slice()),
        Samples::U16(v) => (j::JxlDataType::JXL_TYPE_UINT16, 16, bytemuck::cast_slice(v)),
        Samples::F32(_) => return Err(EncodeError::Malformed),
    };
    let gray = img.channels.is_gray();
    let alpha = img.channels.has_alpha();

    unsafe {
        let s = Session {
            enc: j::JxlEncoderCreate(std::ptr::null()),
            runner: j::JxlThreadParallelRunnerCreate(std::ptr::null(), threads()),
        };
        if s.enc.is_null() || s.runner.is_null() {
            return Err(codec("out of memory"));
        }
        ok(
            j::JxlEncoderSetParallelRunner(s.enc, Some(j::JxlThreadParallelRunner), s.runner),
            "couldn't start its threads",
        )?;

        let mut info = std::mem::MaybeUninit::<j::JxlBasicInfo>::zeroed();
        j::JxlEncoderInitBasicInfo(info.as_mut_ptr());
        let mut info = info.assume_init();
        info.xsize = img.width;
        info.ysize = img.height;
        info.bits_per_sample = bits;
        info.exponent_bits_per_sample = 0;
        info.num_color_channels = if gray { 1 } else { 3 };
        if alpha {
            info.num_extra_channels = 1;
            info.alpha_bits = bits;
            info.alpha_exponent_bits = 0;
        }
        // Lossless keeps the samples in their own space; lossy works in XYB and records the
        // original space so the decoder returns to it.
        info.uses_original_profile = lossless as i32;
        ok(
            j::JxlEncoderSetBasicInfo(s.enc, &info),
            "image too large or unsupported",
        )?;

        match &img.color {
            OutputColor::Srgb => {
                let mut ce = std::mem::MaybeUninit::<j::JxlColorEncoding>::zeroed();
                j::JxlColorEncodingSetToSRGB(ce.as_mut_ptr(), gray as i32);
                ok(
                    j::JxlEncoderSetColorEncoding(s.enc, ce.as_ptr()),
                    "color setup failed",
                )?;
            }
            OutputColor::Icc(p) => ok(
                j::JxlEncoderSetICCProfile(s.enc, p.as_ptr(), p.len()),
                "couldn't use the color profile",
            )?,
        }

        let fs = j::JxlEncoderFrameSettingsCreate(s.enc, std::ptr::null());
        if fs.is_null() {
            return Err(codec("out of memory"));
        }
        ok(
            j::JxlEncoderFrameSettingsSetOption(
                fs,
                j::JxlEncoderFrameSettingId::JXL_ENC_FRAME_SETTING_EFFORT,
                effort.clamp(1, 10),
            ),
            "bad effort",
        )?;
        if lossless {
            ok(
                j::JxlEncoderSetFrameLossless(fs, 1),
                "lossless setup failed",
            )?;
        } else {
            // Quality 100 maps to distance 0, which libjxl takes as "switch to lossless" — and
            // refuses here, the lossy setup having left the original profile off. Use its
            // smallest lossy distance, 0.05 (it raises anything smaller to that anyway).
            let distance = j::JxlEncoderDistanceFromQuality(quality.clamp(1, 100) as f32);
            ok(
                j::JxlEncoderSetFrameDistance(fs, distance.max(0.05)),
                "bad quality",
            )?;
        }

        let format = j::JxlPixelFormat {
            num_channels: img.channels.count() as u32,
            data_type,
            endianness: j::JxlEndianness::JXL_NATIVE_ENDIAN,
            align: 0,
        };
        ok(
            j::JxlEncoderAddImageFrame(fs, &format, bytes.as_ptr().cast(), bytes.len()),
            "couldn't take the pixels",
        )?;
        j::JxlEncoderCloseInput(s.enc);

        let mut out = vec![0u8; (bytes.len() / 8).clamp(64 * 1024, 64 << 20)];
        let mut used = 0usize;
        loop {
            let mut next = out.as_mut_ptr().add(used);
            let mut avail = out.len() - used;
            let status = j::JxlEncoderProcessOutput(s.enc, &mut next, &mut avail);
            used = out.len() - avail;
            match status {
                j::JxlEncoderStatus::JXL_ENC_SUCCESS => break,
                j::JxlEncoderStatus::JXL_ENC_NEED_MORE_OUTPUT => out.resize(out.len() * 2, 0),
                _ => return Err(codec(format!("{:?}", j::JxlEncoderGetError(s.enc)))),
            }
        }
        out.truncate(used);
        Ok(out)
    }
}

/// The decoder and its thread pool, destroyed together.
struct DecodeSession {
    dec: *mut j::JxlDecoder,
    runner: *mut c_void,
}

impl Drop for DecodeSession {
    fn drop(&mut self) {
        unsafe {
            if !self.dec.is_null() {
                j::JxlDecoderDestroy(self.dec);
            }
            if !self.runner.is_null() {
                j::JxlThreadParallelRunnerDestroy(self.runner);
            }
        }
    }
}

/// Decode a JPEG XL to 8-bit RGBA with libjxl: `(width, height, pixels, icc)`, the profile
/// describing the pixels (libjxl renders a lossy file back into its original space with its
/// own CMS). Export mode's preview reads lossy JPEG XL back with this rather than the viewer's
/// jxl-oxide, which squeezes colours outside sRGB into it on the way to a wide-gamut target —
/// a Display P3 photo's JPEG XL would preview duller than the file is, and than Preview,
/// Safari or any libjxl-based viewer shows it.
pub fn decode_rgba8(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>, Vec<u8>), EncodeError> {
    // A profile libjxl's CMS can't render into: decode as libjxl would by default (sRGB)
    // rather than not at all.
    decode_rgba8_in(bytes, true).or_else(|_| decode_rgba8_in(bytes, false))
}

fn decode_rgba8_in(
    bytes: &[u8],
    own_space: bool,
) -> Result<(u32, u32, Vec<u8>, Vec<u8>), EncodeError> {
    let fail = |what: &str| EncodeError::Codec(format!("jpeg xl: {what}"));
    unsafe {
        let s = DecodeSession {
            dec: j::JxlDecoderCreate(std::ptr::null()),
            runner: j::JxlThreadParallelRunnerCreate(std::ptr::null(), threads()),
        };
        if s.dec.is_null() || s.runner.is_null() {
            return Err(fail("out of memory"));
        }
        let events = j::JxlDecoderStatus::JXL_DEC_BASIC_INFO as i32
            | j::JxlDecoderStatus::JXL_DEC_COLOR_ENCODING as i32
            | j::JxlDecoderStatus::JXL_DEC_FULL_IMAGE as i32;
        // libjxl's own CMS (skcms), for rendering a lossy file into an ICC-described space.
        j::JxlDecoderSetCms(s.dec, *j::JxlGetDefaultCms());
        if j::JxlDecoderSubscribeEvents(s.dec, events) != j::JxlDecoderStatus::JXL_DEC_SUCCESS
            || j::JxlDecoderSetParallelRunner(s.dec, Some(j::JxlThreadParallelRunner), s.runner)
                != j::JxlDecoderStatus::JXL_DEC_SUCCESS
            || j::JxlDecoderSetInput(s.dec, bytes.as_ptr(), bytes.len())
                != j::JxlDecoderStatus::JXL_DEC_SUCCESS
        {
            return Err(fail("couldn't start decoding"));
        }
        j::JxlDecoderCloseInput(s.dec);
        let format = j::JxlPixelFormat {
            num_channels: 4,
            data_type: j::JxlDataType::JXL_TYPE_UINT8,
            endianness: j::JxlEndianness::JXL_NATIVE_ENDIAN,
            align: 0,
        };
        let (mut w, mut h) = (0u32, 0u32);
        let (mut px, mut icc) = (Vec::new(), Vec::new());
        loop {
            match j::JxlDecoderProcessInput(s.dec) {
                j::JxlDecoderStatus::JXL_DEC_BASIC_INFO => {
                    let mut info = std::mem::MaybeUninit::<j::JxlBasicInfo>::zeroed();
                    if j::JxlDecoderGetBasicInfo(s.dec, info.as_mut_ptr())
                        != j::JxlDecoderStatus::JXL_DEC_SUCCESS
                    {
                        return Err(fail("unreadable header"));
                    }
                    let info = info.assume_init();
                    (w, h) = (info.xsize, info.ysize);
                }
                j::JxlDecoderStatus::JXL_DEC_COLOR_ENCODING if !own_space => {
                    let target = j::JxlColorProfileTarget::JXL_COLOR_PROFILE_TARGET_DATA;
                    let mut n = 0usize;
                    if j::JxlDecoderGetICCProfileSize(s.dec, target, &mut n)
                        == j::JxlDecoderStatus::JXL_DEC_SUCCESS
                    {
                        icc = vec![0; n];
                        if j::JxlDecoderGetColorAsICCProfile(s.dec, target, icc.as_mut_ptr(), n)
                            != j::JxlDecoderStatus::JXL_DEC_SUCCESS
                        {
                            icc.clear();
                        }
                    }
                }
                j::JxlDecoderStatus::JXL_DEC_COLOR_ENCODING => {
                    // A lossy (XYB) file would otherwise come out as linear sRGB, clipped to
                    // 8 bits — a P3 red flattened to sRGB's. Ask for its own space: by name
                    // when libjxl knows it (sRGB, P3, …), else as its ICC profile through
                    // libjxl's CMS. A lossless file is already in its space; both calls then
                    // fail harmlessly and change nothing.
                    let original_target =
                        j::JxlColorProfileTarget::JXL_COLOR_PROFILE_TARGET_ORIGINAL;
                    let mut original = std::mem::MaybeUninit::<j::JxlColorEncoding>::zeroed();
                    if j::JxlDecoderGetColorAsEncodedProfile(
                        s.dec,
                        original_target,
                        original.as_mut_ptr(),
                    ) == j::JxlDecoderStatus::JXL_DEC_SUCCESS
                    {
                        let _ = j::JxlDecoderSetPreferredColorProfile(s.dec, original.as_ptr());
                    } else {
                        let mut n = 0usize;
                        if j::JxlDecoderGetICCProfileSize(s.dec, original_target, &mut n)
                            == j::JxlDecoderStatus::JXL_DEC_SUCCESS
                        {
                            let mut profile = vec![0u8; n];
                            if j::JxlDecoderGetColorAsICCProfile(
                                s.dec,
                                original_target,
                                profile.as_mut_ptr(),
                                n,
                            ) == j::JxlDecoderStatus::JXL_DEC_SUCCESS
                            {
                                let _ = j::JxlDecoderSetOutputColorProfile(
                                    s.dec,
                                    std::ptr::null(),
                                    profile.as_ptr(),
                                    profile.len(),
                                );
                            }
                        }
                    }
                    let target = j::JxlColorProfileTarget::JXL_COLOR_PROFILE_TARGET_DATA;
                    let mut n = 0usize;
                    if j::JxlDecoderGetICCProfileSize(s.dec, target, &mut n)
                        == j::JxlDecoderStatus::JXL_DEC_SUCCESS
                    {
                        icc = vec![0; n];
                        if j::JxlDecoderGetColorAsICCProfile(s.dec, target, icc.as_mut_ptr(), n)
                            != j::JxlDecoderStatus::JXL_DEC_SUCCESS
                        {
                            icc.clear();
                        }
                    }
                }
                j::JxlDecoderStatus::JXL_DEC_NEED_IMAGE_OUT_BUFFER => {
                    let mut n = 0usize;
                    if j::JxlDecoderImageOutBufferSize(s.dec, &format, &mut n)
                        != j::JxlDecoderStatus::JXL_DEC_SUCCESS
                        || n != (w as usize) * (h as usize) * 4
                    {
                        return Err(fail("unexpected image size"));
                    }
                    px = vec![0u8; n];
                    if j::JxlDecoderSetImageOutBuffer(s.dec, &format, px.as_mut_ptr().cast(), n)
                        != j::JxlDecoderStatus::JXL_DEC_SUCCESS
                    {
                        return Err(fail("couldn't take the pixels"));
                    }
                }
                j::JxlDecoderStatus::JXL_DEC_FULL_IMAGE => {}
                j::JxlDecoderStatus::JXL_DEC_SUCCESS => break,
                _ => return Err(fail("corrupt file")),
            }
        }
        if px.is_empty() {
            return Err(fail("no image"));
        }
        Ok((w, h, px, icc))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Channels;

    fn image(
        w: u32,
        h: u32,
        channels: Channels,
        samples: Samples,
        color: OutputColor,
    ) -> ExportImage {
        ExportImage {
            width: w,
            height: h,
            channels,
            samples,
            color,
            notes: Vec::new(),
        }
    }

    /// Decode with libjxl itself to the given pixel format: (pixels, ICC profile).
    fn decode(bytes: &[u8], channels: u32, sixteen: bool) -> (Vec<u8>, Vec<u8>) {
        unsafe {
            let dec = j::JxlDecoderCreate(std::ptr::null());
            assert_eq!(
                j::JxlDecoderSubscribeEvents(
                    dec,
                    (j::JxlDecoderStatus::JXL_DEC_COLOR_ENCODING as i32)
                        | (j::JxlDecoderStatus::JXL_DEC_FULL_IMAGE as i32)
                ),
                j::JxlDecoderStatus::JXL_DEC_SUCCESS
            );
            j::JxlDecoderSetInput(dec, bytes.as_ptr(), bytes.len());
            j::JxlDecoderCloseInput(dec);
            let format = j::JxlPixelFormat {
                num_channels: channels,
                data_type: if sixteen {
                    j::JxlDataType::JXL_TYPE_UINT16
                } else {
                    j::JxlDataType::JXL_TYPE_UINT8
                },
                endianness: j::JxlEndianness::JXL_NATIVE_ENDIAN,
                align: 0,
            };
            let (mut px, mut icc) = (Vec::new(), Vec::new());
            loop {
                match j::JxlDecoderProcessInput(dec) {
                    j::JxlDecoderStatus::JXL_DEC_COLOR_ENCODING => {
                        let mut n = 0usize;
                        j::JxlDecoderGetICCProfileSize(
                            dec,
                            j::JxlColorProfileTarget::JXL_COLOR_PROFILE_TARGET_DATA,
                            &mut n,
                        );
                        icc = vec![0; n];
                        j::JxlDecoderGetColorAsICCProfile(
                            dec,
                            j::JxlColorProfileTarget::JXL_COLOR_PROFILE_TARGET_DATA,
                            icc.as_mut_ptr(),
                            n,
                        );
                    }
                    j::JxlDecoderStatus::JXL_DEC_NEED_IMAGE_OUT_BUFFER => {
                        let mut n = 0usize;
                        j::JxlDecoderImageOutBufferSize(dec, &format, &mut n);
                        px = vec![0; n];
                        j::JxlDecoderSetImageOutBuffer(dec, &format, px.as_mut_ptr().cast(), n);
                    }
                    j::JxlDecoderStatus::JXL_DEC_FULL_IMAGE => {}
                    j::JxlDecoderStatus::JXL_DEC_SUCCESS => break,
                    other => panic!("decode: {other:?}"),
                }
            }
            j::JxlDecoderDestroy(dec);
            (px, icc)
        }
    }

    /// Lossless is exact at 16 bits with alpha, and the profile comes back byte for byte.
    #[test]
    fn lossless_16_bit_with_alpha_is_exact_and_keeps_the_profile() {
        let p3 = crate::icc::display_p3_profile();
        let px: Vec<u16> = (0..20 * 10 * 4)
            .map(|i| (i as u16).wrapping_mul(977))
            .collect();
        let jxl = encode_jxl(
            &image(
                20,
                10,
                Channels::Rgba,
                Samples::U16(px.clone()),
                OutputColor::Icc(p3.clone()),
            ),
            90,
            true,
        )
        .unwrap();
        assert!(
            jxl.starts_with(&[0xFF, 0x0A]) || jxl.starts_with(b"\0\0\0\x0CJXL "),
            "a codestream or the container"
        );
        let (back, icc) = decode(&jxl, 4, true);
        assert_eq!(bytemuck::cast_slice::<u8, u16>(&back), px.as_slice());
        assert_eq!(icc, p3);
    }

    /// Lossy lands close, gets smaller with lower quality, and gray stays gray.
    #[test]
    fn lossy_is_close_and_quality_trades_size() {
        let px: Vec<u8> = (0..64 * 48)
            .flat_map(|i| [(i % 64 * 4) as u8, (i / 64 * 5) as u8, 120])
            .collect();
        let src = image(
            64,
            48,
            Channels::Rgb,
            Samples::U8(px.clone()),
            OutputColor::Srgb,
        );
        let hi = encode_jxl(&src, 95, false).unwrap();
        let lo = encode_jxl(&src, 30, false).unwrap();
        assert!(lo.len() < hi.len(), "{} < {}", lo.len(), hi.len());
        let (back, _) = decode(&hi, 3, false);
        let worst = back
            .iter()
            .zip(&px)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        assert!(worst < 16, "max error {worst}");

        let gray = image(
            8,
            8,
            Channels::Gray,
            Samples::U8(vec![90; 64]),
            OutputColor::Srgb,
        );
        let (back, _) = decode(&encode_jxl(&gray, 90, true).unwrap(), 1, false);
        assert_eq!(back, vec![90; 64]);
    }

    /// The slider's top end works (libjxl refuses distance 0 for lossy), and libjxl reads a
    /// lossy P3 file back into P3 with its saturated red intact — the gamut jxl-oxide loses
    /// (see `decode_rgba8`).
    #[test]
    fn quality_100_works_and_libjxl_reads_p3_back_into_p3() {
        let p3 = crate::icc::display_p3_profile();
        let red = image(
            16,
            16,
            Channels::Rgb,
            Samples::U8([255, 0, 0].repeat(256)),
            OutputColor::Icc(p3),
        );
        for q in [90u8, 100] {
            let jxl = encode_jxl(&red, q, false).unwrap_or_else(|e| panic!("q{q}: {e}"));
            let (w, h, px, icc) = decode_rgba8(&jxl).unwrap();
            assert_eq!((w, h), (16, 16));
            let c = &px[(8 * 16 + 8) * 4..][..4];
            assert!(
                c[0] > 245 && c[1] < 10 && c[2] < 10 && c[3] == 255,
                "q{q}: {c:?}"
            );
            let back = moxcms::ColorProfile::new_from_slice(&icc).unwrap();
            assert!(
                (back.red_colorant.x - 0.515).abs() < 0.01,
                "a P3 profile: {back:?}"
            );
        }
        assert!(decode_rgba8(b"not a jxl").is_err());
    }

    /// A profile libjxl can't name (Adobe RGB, from the Mac's own set) still reads back into
    /// its own space, through libjxl's CMS: Adobe RGB's pure green stays pure green.
    #[test]
    fn an_arbitrary_profile_reads_back_into_its_own_space() {
        let Ok(adobe) = std::fs::read("/System/Library/ColorSync/Profiles/AdobeRGB1998.icc") else {
            return; // not a Mac
        };
        let green = image(
            16,
            16,
            Channels::Rgb,
            Samples::U8([0, 255, 0].repeat(256)),
            OutputColor::Icc(adobe),
        );
        let jxl = encode_jxl(&green, 90, false).unwrap();
        let (_, _, px, icc) = decode_rgba8(&jxl).unwrap();
        let c = &px[(8 * 16 + 8) * 4..][..3];
        assert!(c[0] < 12 && c[1] > 243 && c[2] < 12, "{c:?}");
        let back = moxcms::ColorProfile::new_from_slice(&icc).unwrap();
        let ours = moxcms::ColorProfile::new_from_slice(
            &std::fs::read("/System/Library/ColorSync/Profiles/AdobeRGB1998.icc").unwrap(),
        )
        .unwrap();
        assert!(
            (back.green_colorant.y - ours.green_colorant.y).abs() < 0.01,
            "Adobe RGB, not sRGB"
        );
    }

    #[test]
    fn refuses_what_it_cannot_write() {
        let short = image(
            4,
            4,
            Channels::Rgb,
            Samples::U8(vec![0; 5]),
            OutputColor::Srgb,
        );
        assert_eq!(encode_jxl(&short, 90, false), Err(EncodeError::Malformed));
        let float = image(
            1,
            1,
            Channels::Rgb,
            Samples::F32(vec![0.0; 3]),
            OutputColor::Srgb,
        );
        assert_eq!(encode_jxl(&float, 90, false), Err(EncodeError::Malformed));
        assert!(JxlEncoder {
            quality: 90,
            lossless: true
        }
        .lossless());
        assert_eq!(
            JxlEncoder {
                quality: 90,
                lossless: false
            }
            .extension(),
            "jxl"
        );
    }
}
