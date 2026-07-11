//! Windows AV1 decode backend for animated AVIF (`avis`) playback — task #76.
//!
//! Everything dav1d goes through the C accessor shim (`csrc/dav1d_shim.c`),
//! compiled by build.rs against the pinned vcpkg tree's headers — dav1d's
//! public structs never cross the FFI boundary, so there is no hand-mirrored
//! layout to drift (see the task 76 plan, "ABI safety"). This module is the
//! Rust half: RAII wrappers ([`Decoder`], [`Data`], [`Picture`]) over the
//! shim's opaque pointers, so every error exit releases dav1d's references.
//!
//! The send/get loop contract (plan, "send_data semantics"): [`Decoder::send`]
//! either consumes the data or asks the caller to drain pictures first — and
//! dav1d may advance a multi-TU buffer in place across `Again`, so callers loop
//! on [`Data::remaining`], re-sending the *same* [`Data`]. After the last
//! sample, call [`Decoder::next_picture`] until `None` to flush delayed frames.
//!
//! Wired into `decode_animation` by plan phases 4-6 (demux → decode → YUV→RGB);
//! until then only the unit tests consume this surface.
#![allow(dead_code)]

use std::ffi::CStr;
use std::marker::PhantomData;
use std::os::raw::{c_char, c_int};
use std::ptr::NonNull;

use crate::DecodeError;

// Opaque shim handles — zero-sized so they can only exist behind a pointer.
#[repr(C)]
struct RawContext {
    _opaque: [u8; 0],
}
#[repr(C)]
struct RawData {
    _opaque: [u8; 0],
}
#[repr(C)]
struct RawPicture {
    _opaque: [u8; 0],
}

extern "C" {
    fn pb_dav1d_version() -> *const c_char;
    fn pb_dav1d_version_ok() -> c_int;

    fn pb_dav1d_open(n_threads: c_int, max_frame_delay: c_int) -> *mut RawContext;
    fn pb_dav1d_close(c: *mut RawContext);

    fn pb_dav1d_data_new(ptr: *const u8, len: usize, cookie: i64) -> *mut RawData;
    fn pb_dav1d_data_remaining(d: *const RawData) -> usize;
    fn pb_dav1d_data_free(d: *mut RawData);
    fn pb_dav1d_send(c: *mut RawContext, d: *mut RawData) -> c_int;
    fn pb_dav1d_err_is_again(rc: c_int) -> c_int;

    fn pb_dav1d_picture_new() -> *mut RawPicture;
    fn pb_dav1d_get_picture(c: *mut RawContext, p: *mut RawPicture) -> c_int;
    fn pb_dav1d_picture_free(p: *mut RawPicture);
    fn pb_dav1d_picture_width(p: *const RawPicture) -> c_int;
    fn pb_dav1d_picture_height(p: *const RawPicture) -> c_int;
    fn pb_dav1d_picture_layout(p: *const RawPicture) -> c_int;
    fn pb_dav1d_picture_bpc(p: *const RawPicture) -> c_int;
    fn pb_dav1d_picture_plane(p: *const RawPicture, plane: c_int) -> *const u8;
    fn pb_dav1d_picture_stride(p: *const RawPicture, plane: c_int) -> isize;
    fn pb_dav1d_picture_cookie(p: *const RawPicture) -> i64;
    fn pb_dav1d_picture_matrix(p: *const RawPicture) -> c_int;
    fn pb_dav1d_picture_primaries(p: *const RawPicture) -> c_int;
    fn pb_dav1d_picture_transfer(p: *const RawPicture) -> c_int;
    fn pb_dav1d_picture_full_range(p: *const RawPicture) -> c_int;
    fn pb_dav1d_picture_chroma_pos(p: *const RawPicture) -> c_int;
}

/// The linked dav1d's version string (e.g. `"1.5.3"`, the vcpkg pin).
pub fn version() -> &'static str {
    // SAFETY: dav1d_version() returns a pointer to a static NUL-terminated
    // string inside the linked library; it is valid for the program's lifetime.
    unsafe { CStr::from_ptr(pb_dav1d_version()) }
        .to_str()
        .unwrap_or("<non-utf8>")
}

/// Whether the linked lib's API major matches the headers the shim was
/// compiled against. `false` means a mixed/stale vcpkg tree; [`Decoder::open`]
/// refuses to run rather than risk a struct-layout mismatch.
pub fn version_ok() -> bool {
    unsafe { pb_dav1d_version_ok() != 0 }
}

/// `Dav1dPixelLayout` values surfaced by [`Picture::layout`]. Shaped into a
/// proper enum by the YUV→RGB stage (plan phase 6).
pub const LAYOUT_I400: i32 = 0;
pub const LAYOUT_I420: i32 = 1;
pub const LAYOUT_I422: i32 = 2;
pub const LAYOUT_I444: i32 = 3;

/// Outcome of [`Decoder::send`].
#[derive(Debug, PartialEq, Eq)]
pub enum SendStatus {
    /// The data was fully consumed ([`Data::remaining`] is now 0).
    Consumed,
    /// dav1d is backed up: drain via [`Decoder::next_picture`], then re-send
    /// the *same* [`Data`] (which may have been advanced in place).
    Again,
}

/// An open dav1d decoder. Owned by one decode job (a pool worker thread), so
/// `Send` but not `Sync` — dav1d contexts must not be shared concurrently.
pub struct Decoder {
    ctx: NonNull<RawContext>,
}

// SAFETY: the context is used from exactly one thread at a time; dav1d has no
// thread affinity (its own worker pool is internal).
unsafe impl Send for Decoder {}

impl Decoder {
    /// Open a decoder. `n_threads` / `max_frame_delay` ≤ 0 keep dav1d's
    /// defaults; the animation backend passes small explicit values because
    /// the decode pool already parallelizes across images (plan phase 5).
    pub fn open(n_threads: i32, max_frame_delay: i32) -> Result<Self, DecodeError> {
        if !version_ok() {
            return Err(DecodeError::Corrupt(format!(
                "dav1d version mismatch (linked {}): stale vcpkg tree — re-run \
                 scripts/setup-libheif.ps1",
                version()
            )));
        }
        NonNull::new(unsafe { pb_dav1d_open(n_threads, max_frame_delay) })
            .map(|ctx| Self { ctx })
            .ok_or_else(|| DecodeError::Corrupt("dav1d_open failed".into()))
    }

    /// Feed a temporal unit. On [`SendStatus::Again`] drain pictures and
    /// re-send the same `data` until [`Data::remaining`] hits 0.
    pub fn send(&mut self, data: &mut Data<'_>) -> Result<SendStatus, DecodeError> {
        let rc = unsafe { pb_dav1d_send(self.ctx.as_ptr(), data.ptr.as_ptr()) };
        if rc == 0 {
            Ok(SendStatus::Consumed)
        } else if unsafe { pb_dav1d_err_is_again(rc) } != 0 {
            Ok(SendStatus::Again)
        } else {
            Err(DecodeError::Corrupt(format!(
                "dav1d_send_data failed ({rc})"
            )))
        }
    }

    /// The next decoded picture, `None` when dav1d needs more input (or, after
    /// the last sample, when the delayed-frame queue is fully drained).
    pub fn next_picture(&mut self) -> Result<Option<Picture>, DecodeError> {
        let raw = NonNull::new(unsafe { pb_dav1d_picture_new() })
            .ok_or_else(|| DecodeError::Corrupt("dav1d picture alloc failed".into()))?;
        let rc = unsafe { pb_dav1d_get_picture(self.ctx.as_ptr(), raw.as_ptr()) };
        if rc == 0 {
            Ok(Some(Picture { ptr: raw }))
        } else {
            // Free the never-filled shell (unref of a zeroed picture is a no-op).
            unsafe { pb_dav1d_picture_free(raw.as_ptr()) };
            if unsafe { pb_dav1d_err_is_again(rc) } != 0 {
                Ok(None)
            } else {
                Err(DecodeError::Corrupt(format!(
                    "dav1d_get_picture failed ({rc})"
                )))
            }
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { pb_dav1d_close(self.ctx.as_ptr()) };
    }
}

/// One sample's bytes wrapped for dav1d **without copying** — borrows the
/// source buffer, so the borrow checker enforces the shim's lifetime contract
/// (the no-op free callback relies on the buffer outliving the decode).
/// Dropping releases dav1d's reference even after partial consumption, so a
/// cancelled decode leaks nothing.
pub struct Data<'a> {
    ptr: NonNull<RawData>,
    _bytes: PhantomData<&'a [u8]>,
}

impl<'a> Data<'a> {
    /// Wrap `bytes` (one temporal unit). `cookie` is the sample index, read
    /// back from the decoded picture via [`Picture::cookie`] — the robust
    /// sample→picture mapping (a TU can legally yield zero pictures).
    pub fn new(bytes: &'a [u8], cookie: i64) -> Result<Self, DecodeError> {
        NonNull::new(unsafe { pb_dav1d_data_new(bytes.as_ptr(), bytes.len(), cookie) })
            .map(|ptr| Self {
                ptr,
                _bytes: PhantomData,
            })
            .ok_or_else(|| DecodeError::Corrupt("dav1d_data_wrap failed".into()))
    }

    /// Bytes dav1d has not consumed yet (0 once fully sent).
    pub fn remaining(&self) -> usize {
        unsafe { pb_dav1d_data_remaining(self.ptr.as_ptr()) }
    }
}

impl Drop for Data<'_> {
    fn drop(&mut self) {
        unsafe { pb_dav1d_data_free(self.ptr.as_ptr()) };
    }
}

/// A decoded planar YUV picture. Holds a dav1d frame buffer (and pins decoder
/// pool memory) until dropped — convert to RGB and drop promptly (plan
/// phase 5's memory bounds).
pub struct Picture {
    ptr: NonNull<RawPicture>,
}

// SAFETY: the picture's buffer is immutable after decode and freed on drop.
unsafe impl Send for Picture {}

impl Picture {
    pub fn width(&self) -> u32 {
        unsafe { pb_dav1d_picture_width(self.ptr.as_ptr()) }.max(0) as u32
    }

    pub fn height(&self) -> u32 {
        unsafe { pb_dav1d_picture_height(self.ptr.as_ptr()) }.max(0) as u32
    }

    /// One of the `LAYOUT_*` constants.
    pub fn layout(&self) -> i32 {
        unsafe { pb_dav1d_picture_layout(self.ptr.as_ptr()) }
    }

    /// Bits per component (8/10/12). Planes hold `u8` at 8, little-endian
    /// `u16` above; strides are always in **bytes**.
    pub fn bpc(&self) -> i32 {
        unsafe { pb_dav1d_picture_bpc(self.ptr.as_ptr()) }
    }

    /// Base pointer + byte stride for plane 0..3 (0 = Y; 1/2 = chroma, absent
    /// for I400). `None` if dav1d has no such plane.
    pub fn plane(&self, plane: i32) -> Option<(*const u8, isize)> {
        let p = unsafe { pb_dav1d_picture_plane(self.ptr.as_ptr(), plane) };
        if p.is_null() {
            None
        } else {
            Some((p, unsafe {
                pb_dav1d_picture_stride(self.ptr.as_ptr(), plane)
            }))
        }
    }

    /// The sample-index cookie from [`Data::new`].
    pub fn cookie(&self) -> i64 {
        unsafe { pb_dav1d_picture_cookie(self.ptr.as_ptr()) }
    }

    /// CICP matrix coefficients from the sequence header (-1 if absent).
    pub fn matrix(&self) -> i32 {
        unsafe { pb_dav1d_picture_matrix(self.ptr.as_ptr()) }
    }

    /// CICP color primaries from the sequence header (-1 if absent).
    pub fn primaries(&self) -> i32 {
        unsafe { pb_dav1d_picture_primaries(self.ptr.as_ptr()) }
    }

    /// CICP transfer characteristics from the sequence header (-1 if absent).
    /// The decode-time HDR backstop checks this for PQ/HLG (plan phase 6).
    pub fn transfer(&self) -> i32 {
        unsafe { pb_dav1d_picture_transfer(self.ptr.as_ptr()) }
    }

    /// True = full range; false = limited (or unknown, the safe default).
    pub fn full_range(&self) -> bool {
        unsafe { pb_dav1d_picture_full_range(self.ptr.as_ptr()) != 0 }
    }

    /// `Dav1dChromaSamplePosition`: 0 unknown, 1 vertical, 2 colocated.
    pub fn chroma_pos(&self) -> i32 {
        unsafe { pb_dav1d_picture_chroma_pos(self.ptr.as_ptr()) }
    }
}

impl Drop for Picture {
    fn drop(&mut self) {
        unsafe { pb_dav1d_picture_free(self.ptr.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shim + static lib link and agree on the API version, and the runtime
    /// version is the one the vcpkg pin promises (setup-libheif.ps1 -VcpkgRef).
    /// If the pin moves, update the expected prefix here deliberately.
    #[test]
    fn linked_dav1d_is_the_pinned_version() {
        assert!(
            version_ok(),
            "shim headers vs linked lib API-major mismatch"
        );
        assert!(
            version().starts_with("1.5"),
            "vcpkg pin moved? linked dav1d = {}",
            version()
        );
    }

    /// Phase-3 acceptance: a real keyframe temporal unit (64×64 solid red,
    /// libaom via ffmpeg, IVF-stripped) round-trips to a decoded picture with
    /// the right geometry and genuinely red pixels. Exercises the full
    /// send/drain loop contract rather than assuming one-send-one-picture.
    #[test]
    fn keyframe_obu_roundtrips_to_a_red_picture() {
        let obu: &[u8] = include_bytes!("../tests/fixtures/red_64x64_keyframe.obu");
        let mut dec = Decoder::open(1, 1).expect("open decoder");
        let mut data = Data::new(obu, 42).expect("wrap TU");

        let mut pic = None;
        for _ in 0..16 {
            if data.remaining() > 0 {
                // Again → drain below, then re-send the same (possibly
                // advanced) data; Consumed → remaining() is now 0.
                let _ = dec.send(&mut data).expect("send TU");
            }
            if let Some(p) = dec.next_picture().expect("get picture") {
                pic = Some(p);
                break;
            }
        }
        let pic = pic.expect("no picture after 16 send/drain iterations");

        assert_eq!((pic.width(), pic.height()), (64, 64));
        assert_eq!(pic.layout(), LAYOUT_I420);
        assert_eq!(pic.bpc(), 8);
        assert_eq!(pic.cookie(), 42, "sample cookie must ride through dav1d");

        // Solid red in limited-range BT.601-ish YUV: Y ≈ 81, Cr ≈ 240. Wide
        // tolerances absorb encoder/matrix variance while still proving these
        // are decoded red pixels, not zeroed memory.
        let (y_ptr, y_stride) = pic.plane(0).expect("luma plane");
        let (v_ptr, v_stride) = pic.plane(2).expect("Cr plane");
        assert!(y_stride >= 64 && v_stride >= 32, "sane strides");
        // SAFETY: 64×64 I420 picture — row 32/16 exists in each plane.
        let y_center = unsafe { *y_ptr.offset(32 * y_stride + 32) };
        let v_center = unsafe { *v_ptr.offset(16 * v_stride + 16) };
        assert!((60..=110).contains(&y_center), "red luma, got Y={y_center}");
        assert!(v_center > 200, "red chroma, got Cr={v_center}");
    }

    /// Garbage input surfaces as a clean `DecodeError` (never a panic), and
    /// every wrapper drops safely afterwards — the RAII error-exit contract.
    #[test]
    fn garbage_bytes_error_cleanly_and_drop_safely() {
        let junk = [0xFFu8; 64];
        let mut dec = Decoder::open(1, 1).expect("open decoder");
        let mut data = Data::new(&junk, 7).expect("wrap junk");
        // dav1d may reject at send or at the first get — either way it must be
        // an Err (or produce nothing), and the drops below must be clean.
        let mut produced = 0;
        for _ in 0..4 {
            if data.remaining() > 0 && dec.send(&mut data).is_err() {
                break;
            }
            match dec.next_picture() {
                Ok(Some(_)) => produced += 1,
                Ok(None) => {}
                Err(_) => break,
            }
        }
        assert_eq!(produced, 0, "garbage must not decode to a picture");
        // `dec` and `data` drop here — close/unref on the error path.
    }

    /// Dropping a decoder with an undrained picture queue (the cancellation
    /// shape: navigate away mid-sequence) releases everything without a crash.
    #[test]
    fn dropping_mid_decode_is_clean() {
        let obu: &[u8] = include_bytes!("../tests/fixtures/red_64x64_keyframe.obu");
        let mut dec = Decoder::open(1, 1).expect("open decoder");
        let mut data = Data::new(obu, 0).expect("wrap TU");
        let _ = dec.send(&mut data).expect("send TU");
        // Deliberately no drain: Decoder (with internal state) and Data drop now.
    }
}
