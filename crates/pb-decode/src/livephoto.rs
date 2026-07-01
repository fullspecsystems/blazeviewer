//! Apple **Live Photo** motion decoder (macOS), via AVFoundation — task #38 spike.
//!
//! A Live Photo is *two files*: a still (JPEG/HEIC) plus a short QuickTime `.mov`
//! (H.264 on the iPhone 6s, HEVC since), linked by a shared content identifier. This
//! module decodes that companion `.mov` into the same [`Animation`] the animated-image
//! playback consumes, so it reuses the whole task #37 machinery (Playback, eager-prep,
//! present path). Only the *decode* is new — and it leans on the OS decoder, so H.264
//! and HEVC both "just work" with hardware assist and no per-codec Rust.
//!
//! Approach: `AVAssetImageGenerator` sampled at the track's frame rate. It hands back a
//! `CGImage` per frame — already rotated (`appliesPreferredTrackTransform`) and
//! color-managed — which feeds straight into `imageio::draw_cgimage_p3`, exactly like
//! the still HEIC path. RAM is bounded by `maximumSize` (a full-res 3 s clip is ~1 GB
//! of RGBA, so the long edge is capped). Reads only, RAM-only — the no-trace guarantee
//! (privacy #2) holds.
//!
//! Hand-rolled Objective-C runtime FFI, the same no-new-deps style as `imageio.rs`.
//! The only ABI subtlety is passing `CMTime`/`CGSize` by value through `objc_msgSend`:
//! casting it to the correct `extern "C"` fn-pointer type makes Rust apply the same
//! AAPCS64 rules the ObjC method was compiled with, so they line up.
//!
//! Known spike limitations (productionization tracked in #38): frames are sampled at a
//! *constant* `1/nominalFrameRate` (true per-frame timestamps need `AVAssetReader`);
//! audio is out of scope; the whole clip is pre-decoded to RGBA (no streaming yet).

use std::ffi::{c_void, CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::Duration;

use crate::imageio::{draw_cgimage_p3, unpremultiply};
use crate::{AnimFrame, Animation, AnimationKind, ColorTransform, DecodeError};

/// Safety cap on decoded frames — a real Live Photo is ~3 s, so this is only a
/// runaway guard (a malformed/long `.mov`). Hitting it flags the result truncated.
const MAX_MOTION_FRAMES: usize = 600;

/// `kCMTimeFlags_Valid`.
const CM_TIME_VALID: u32 = 1;

// --- Objective-C runtime + framework FFI (no new crate deps) -----------------------

type Id = *mut c_void;
type Class = *mut c_void;
type Sel = *const c_void;
type CGImageRef = *const c_void;

/// `CMTime` — a rational timestamp. Layout matches `<CoreMedia/CMTime.h>`.
#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[link(name = "objc", kind = "dylib")]
extern "C" {
    fn objc_getClass(name: *const std::os::raw::c_char) -> Class;
    fn sel_registerName(name: *const std::os::raw::c_char) -> Sel;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    fn CMTimeMakeWithSeconds(seconds: f64, preferred_timescale: i32) -> CMTime;
}

#[link(name = "AVFoundation", kind = "framework")]
extern "C" {
    static AVMediaTypeVideo: Id;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGImageRelease(image: CGImageRef);
}

// Force-link Foundation so `NSString` / `NSURL` are registered.
#[link(name = "Foundation", kind = "framework")]
extern "C" {}

#[inline]
unsafe fn class(name: &CStr) -> Class {
    objc_getClass(name.as_ptr())
}

#[inline]
unsafe fn sel(name: &CStr) -> Sel {
    sel_registerName(name.as_ptr())
}

// Typed `objc_msgSend` shims — one per call signature, casting the shared entry point
// to the correct `extern "C"` fn pointer so the C ABI marshals args/return correctly.
#[inline]
unsafe fn send(obj: Id, s: Sel) -> Id {
    let f: unsafe extern "C" fn(Id, Sel) -> Id =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    f(obj, s)
}
#[inline]
unsafe fn send_id(obj: Id, s: Sel, a: Id) -> Id {
    let f: unsafe extern "C" fn(Id, Sel, Id) -> Id =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    f(obj, s, a)
}
#[inline]
unsafe fn send_id2(obj: Id, s: Sel, a: Id, b: Id) -> Id {
    let f: unsafe extern "C" fn(Id, Sel, Id, Id) -> Id =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    f(obj, s, a, b)
}
#[inline]
unsafe fn send_cstr(obj: Id, s: Sel, a: *const std::os::raw::c_char) -> Id {
    let f: unsafe extern "C" fn(Id, Sel, *const std::os::raw::c_char) -> Id =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    f(obj, s, a)
}
#[inline]
unsafe fn send_f32(obj: Id, s: Sel) -> f32 {
    let f: unsafe extern "C" fn(Id, Sel) -> f32 =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    f(obj, s)
}
#[inline]
unsafe fn send_bool(obj: Id, s: Sel, a: bool) {
    let f: unsafe extern "C" fn(Id, Sel, bool) =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    f(obj, s, a)
}
#[inline]
unsafe fn send_size(obj: Id, s: Sel, a: CGSize) {
    let f: unsafe extern "C" fn(Id, Sel, CGSize) =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    f(obj, s, a)
}
#[inline]
unsafe fn send_cmtime(obj: Id, s: Sel, a: CMTime) {
    let f: unsafe extern "C" fn(Id, Sel, CMTime) =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    f(obj, s, a)
}
/// `copyCGImageAtTime:actualTime:error:` — CMTime by value, two out-pointers we pass
/// null, returns a `+1` `CGImageRef` (caller releases).
#[inline]
unsafe fn send_copy_cgimage(gen: Id, s: Sel, t: CMTime) -> CGImageRef {
    let f: unsafe extern "C" fn(Id, Sel, CMTime, *mut CMTime, *mut Id) -> CGImageRef =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    f(gen, s, t, std::ptr::null_mut(), std::ptr::null_mut())
}

/// Decode the Live Photo motion `.mov` at `path` into an [`Animation`], capping each
/// frame's long edge to `max_long_edge` px (decode-to-fit → bounds RAM). macOS-only.
pub fn decode_live_motion(path: &Path, max_long_edge: u32) -> Result<Animation, DecodeError> {
    let cpath = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| DecodeError::Corrupt("Live Photo path has interior NUL".into()))?;
    let anim = unsafe {
        let pool = objc_autoreleasePoolPush();
        let r = decode_motion_inner(&cpath, max_long_edge);
        objc_autoreleasePoolPop(pool);
        r
    };
    anim.ok_or_else(|| DecodeError::Corrupt("Live Photo motion decode failed".into()))
}

unsafe fn decode_motion_inner(cpath: &CStr, max_long_edge: u32) -> Option<Animation> {
    // NSURL fileURLWithPath:[NSString stringWithUTF8String:path]
    let path_ns = send_cstr(
        class(c"NSString"),
        sel(c"stringWithUTF8String:"),
        cpath.as_ptr(),
    );
    if path_ns.is_null() {
        return None;
    }
    let url = send_id(class(c"NSURL"), sel(c"fileURLWithPath:"), path_ns);
    if url.is_null() {
        return None;
    }
    // AVURLAsset URLAssetWithURL:options:
    let asset = send_id2(
        class(c"AVURLAsset"),
        sel(c"URLAssetWithURL:options:"),
        url,
        std::ptr::null_mut(),
    );
    if asset.is_null() {
        return None;
    }
    // First video track → nominal frame rate (drives the sample cadence).
    let tracks = send_id(asset, sel(c"tracksWithMediaType:"), AVMediaTypeVideo);
    if tracks.is_null() {
        return None;
    }
    let track = send(tracks, sel(c"firstObject"));
    if track.is_null() {
        return None; // no video track — not a motion clip
    }
    let raw_fps = send_f32(track, sel(c"nominalFrameRate"));
    let fps = if raw_fps > 0.1 { raw_fps } else { 30.0 } as f64;

    // AVAssetImageGenerator, rotation-applied, size-capped, exact frames.
    let generator = send_id(
        send(class(c"AVAssetImageGenerator"), sel(c"alloc")),
        sel(c"initWithAsset:"),
        asset,
    );
    if generator.is_null() {
        return None;
    }
    send_bool(generator, sel(c"setAppliesPreferredTrackTransform:"), true);
    let cap = max_long_edge.max(1) as f64;
    send_size(
        generator,
        sel(c"setMaximumSize:"),
        CGSize {
            width: cap,
            height: cap,
        },
    );
    let zero = CMTime {
        value: 0,
        timescale: 1,
        flags: CM_TIME_VALID,
        epoch: 0,
    };
    send_cmtime(generator, sel(c"setRequestedTimeToleranceBefore:"), zero);
    send_cmtime(generator, sel(c"setRequestedTimeToleranceAfter:"), zero);

    let copy_sel = sel(c"copyCGImageAtTime:actualTime:error:");
    let delay = Duration::from_secs_f64(1.0 / fps);
    let mut frames: Vec<AnimFrame> = Vec::new();
    let mut truncated = false;
    for i in 0..MAX_MOTION_FRAMES {
        let t = CMTimeMakeWithSeconds(i as f64 / fps, 600);
        let cg = send_copy_cgimage(generator, copy_sel, t);
        if cg.is_null() {
            break; // past the end of the asset (or a decode error) — done
        }
        let drawn = draw_cgimage_p3(cg);
        CGImageRelease(cg);
        let Some((mut rgba, w, h)) = drawn else { break };
        unpremultiply(&mut rgba);
        frames.push(AnimFrame {
            rgba,
            width: w,
            height: h,
            delay,
        });
    }
    if frames.len() >= MAX_MOTION_FRAMES {
        truncated = true;
    }
    if frames.is_empty() {
        return None;
    }
    let (width, height) = (frames[0].width, frames[0].height);
    Some(Animation {
        kind: AnimationKind::LivePhoto,
        width,
        height,
        frames,
        // A Live Photo plays once and stops (finite loop = 1), not looping forever.
        loop_count: 1,
        codec: "Live Photo",
        // Drawn into a Display-P3 context (see `draw_cgimage_p3`), so carry the same
        // P3(SMPTE-432) → BT.709 transform the still HEIC path uses.
        color: ColorTransform::from_cicp(12, 13, 0, true),
        truncated,
    })
}
