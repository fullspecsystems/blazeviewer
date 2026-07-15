//! Apple **Live Photo** motion decoder (macOS), via AVFoundation's `AVAssetReader` —
//! tasks #38 / #69.
//!
//! A Live Photo is *two files*: a still (JPEG/HEIC) plus a short QuickTime `.mov`
//! (H.264 on the iPhone 6s, HEVC since), linked by a shared content identifier. This
//! module decodes that companion `.mov` — leaning on the OS decoder, so H.264 and
//! HEVC both "just work" with hardware assist and no per-codec Rust.
//!
//! **Streaming** ([`decode_live_motion_streaming`], task #69): `AVAssetReader` +
//! `AVAssetReaderTrackOutput` pull decoded BGRA pixel buffers **sequentially** — one
//! linear decode pass, presentation-ordered — and each frame is emitted through the
//! [`MotionChunk`] contract as soon as it's converted, so playback starts on the
//! first frame instead of after the whole clip. (The previous spike used
//! `AVAssetImageGenerator.copyCGImageAtTime:` with zero tolerance — a random-access
//! API that re-decodes from the nearest keyframe on *every* call and returned only
//! the finished clip; 1–2 s before first frame on a real Live Photo. Git history has
//! it.) The batch [`decode_live_motion`] is now a thin collector over the stream.
//!
//! - **Timing**: each sample's `CMSampleBufferGetDuration` (real container timing,
//!   clamped to sane bounds), falling back to the track's nominal frame rate.
//! - **Rotation**: `AVAssetReader` does *not* apply `preferredTransform`; the
//!   quadrant is snapped by `common::transform_to_quadrant` and applied by
//!   `common::rotate_rgba`. Non-quadrant transforms (mirrored front-camera clips)
//!   play untransformed — an accepted degradation, chosen over refusing to play.
//! - **Color**: the first pixel buffer's CoreVideo attachments are mapped to CICP
//!   code points (`CVColorPrimaries…`/`CVTransferFunction…GetIntegerCodePointForString`)
//!   and carried as the header's [`ColorTransform`] — BT.709 and P3 clips both come
//!   out honest. HDR (HLG/PQ) motion is SDR-clamped through the 8-bit BGRA path
//!   (known limitation; the still keeps its fp16 scRGB path).
//! - **Decode-to-fit**: the long edge is capped by asking VideoToolbox to scale
//!   (`kCVPixelBufferWidth/HeightKey`); if the request is silently ignored the first
//!   buffer's real size is detected and a per-frame Lanczos fallback kicks in. The
//!   header is always built from *decoded* dimensions, never track metadata.
//!
//! Reads only, RAM-only — the no-trace guarantee (privacy #2) holds.
//!
//! Hand-rolled Objective-C runtime FFI, the same no-new-deps style as `imageio.rs`.
//! The ABI subtlety is by-value structs through `objc_msgSend`: casting the shared
//! entry point to the correct `extern "C"` fn-pointer type makes Rust apply the same
//! AAPCS64 rules the ObjC method was compiled with. `preferredTransform` returns a
//! 48-byte `CGAffineTransform` — an **indirect** struct return, which plain
//! `objc_msgSend` handles on arm64 (x8); x86_64 must go through `objc_msgSend_stret`
//! (wired below, untested — the shipping target is aarch64-apple-darwin).

use std::ffi::{c_void, CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::animation::{MotionCollector, MAX_DECODED_BYTES};
use crate::common::{bgra_to_rgba_tight, downscale_to_fit, rotate_rgba, transform_to_quadrant};
use crate::tracks::{
    self as tracks_mod, AvGroup, MediaBackend, MediaTrack, MediaTrackCatalog, TrackCapability,
    TrackFlags, TrackId, TrackKind, TrackLocator, TrackSet,
};
use crate::video::{VideoDetailsProbe, VideoStreamInfo};
use crate::{AnimFrame, Animation, ColorTransform, DecodeError, FitBox, MotionChunk, MotionHeader};

/// Safety cap on decoded frames — a real Live Photo is ~3 s, so this is only a
/// runaway guard (a malformed/long `.mov`). Hitting it flags the result truncated.
const MAX_MOTION_FRAMES: usize = 600;

/// `kCMTimeFlags_Valid`.
const CM_TIME_VALID: u32 = 1;

/// `kCVPixelFormatType_32BGRA` — fourcc `'BGRA'`, the one RGB layout VideoToolbox
/// always supports.
const PIXEL_BGRA: u32 = 0x4247_5241;

/// `kCVPixelBufferLock_ReadOnly`.
const LOCK_READ_ONLY: u64 = 1;

/// `AVAssetReaderStatus` (NSInteger).
const READER_COMPLETED: isize = 2;
const READER_FAILED: isize = 3;
const READER_CANCELLED: isize = 4;

// --- Objective-C runtime + framework FFI (no new crate deps) -----------------------

type Id = *mut c_void;
type Class = *mut c_void;
type Sel = *const c_void;

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

/// `CGAffineTransform` — 6×f64. Returned **by value** from `preferredTransform`.
#[repr(C)]
#[derive(Clone, Copy)]
struct CGAffineTransform {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    tx: f64,
    ty: f64,
}

#[link(name = "objc", kind = "dylib")]
extern "C" {
    fn objc_getClass(name: *const std::os::raw::c_char) -> Class;
    fn sel_registerName(name: *const std::os::raw::c_char) -> Sel;
    fn objc_msgSend();
    #[cfg(target_arch = "x86_64")]
    fn objc_msgSend_stret();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

#[link(name = "AVFoundation", kind = "framework")]
extern "C" {
    static AVMediaTypeVideo: Id;
    static AVMediaTypeAudio: Id;
    // Media-selection groups + option characteristics (task #98). Always compared as
    // framework constants, never as hardcoded strings: their values are an inconsistent
    // mix of `public.*` and literal `AVMediaCharacteristic*` (phase-0 spike).
    static AVMediaCharacteristicAudible: Id;
    static AVMediaCharacteristicLegible: Id;
    static AVMediaCharacteristicIsAuxiliaryContent: Id;
    static AVMediaCharacteristicContainsOnlyForcedSubtitles: Id;
    static AVMediaCharacteristicTranscribesSpokenDialogForAccessibility: Id;
    static AVMediaCharacteristicDescribesMusicAndSoundForAccessibility: Id;
    static AVMediaCharacteristicDescribesVideoForAccessibility: Id;
}

/// `NSPropertyListBinaryFormat_v1_0`.
const NS_PROPERTY_LIST_BINARY_V1_0: usize = 200;

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    fn CMSampleBufferGetImageBuffer(sbuf: *const c_void) -> *const c_void;
    fn CMSampleBufferGetDuration(sbuf: *const c_void) -> CMTime;
    /// Seconds for a `CMTime` (NaN if indefinite/invalid) — used by the stream probe.
    fn CMTimeGetSeconds(time: CMTime) -> f64;
    /// The FourCC media subtype (codec) of a `CMFormatDescription` (e.g. 'avc1', 'hvc1').
    fn CMFormatDescriptionGetMediaSubType(desc: *const c_void) -> u32;
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    fn CVPixelBufferLockBaseAddress(pb: *const c_void, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(pb: *const c_void, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddress(pb: *const c_void) -> *const c_void;
    fn CVPixelBufferGetBytesPerRow(pb: *const c_void) -> usize;
    fn CVPixelBufferGetWidth(pb: *const c_void) -> usize;
    fn CVPixelBufferGetHeight(pb: *const c_void) -> usize;
    fn CVPixelBufferGetPixelFormatType(pb: *const c_void) -> u32;
    /// +1 (copy rule) — release the returned attachment.
    fn CVBufferCopyAttachment(
        buffer: *const c_void,
        key: *const c_void,
        attachment_mode: *mut u32,
    ) -> *const c_void;
    /// CoreVideo colorimetry CFString → H.273/CICP code point (2 = unspecified).
    fn CVColorPrimariesGetIntegerCodePointForString(s: *const c_void) -> i32;
    fn CVTransferFunctionGetIntegerCodePointForString(s: *const c_void) -> i32;
    static kCVPixelBufferPixelFormatTypeKey: *const c_void;
    static kCVPixelBufferWidthKey: *const c_void;
    static kCVPixelBufferHeightKey: *const c_void;
    static kCVImageBufferColorPrimariesKey: *const c_void;
    static kCVImageBufferTransferFunctionKey: *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: *const c_void);
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
unsafe fn send_void(obj: Id, s: Sel) {
    let f: unsafe extern "C" fn(Id, Sel) =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    f(obj, s)
}
#[inline]
unsafe fn send_void_id(obj: Id, s: Sel, a: Id) {
    let f: unsafe extern "C" fn(Id, Sel, Id) =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    f(obj, s, a)
}
#[inline]
unsafe fn send_void_id2(obj: Id, s: Sel, a: Id, b: Id) {
    let f: unsafe extern "C" fn(Id, Sel, Id, Id) =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    f(obj, s, a, b)
}
#[inline]
unsafe fn send_ret_bool(obj: Id, s: Sel) -> bool {
    let f: unsafe extern "C" fn(Id, Sel) -> bool =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    f(obj, s)
}
#[inline]
unsafe fn send_id_ret_bool(obj: Id, s: Sel, a: Id) -> bool {
    let f: unsafe extern "C" fn(Id, Sel, Id) -> bool =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    f(obj, s, a)
}
#[inline]
unsafe fn send_ret_isize(obj: Id, s: Sel) -> isize {
    let f: unsafe extern "C" fn(Id, Sel) -> isize =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    f(obj, s)
}
#[inline]
unsafe fn send_u32(obj: Id, s: Sel, a: u32) -> Id {
    let f: unsafe extern "C" fn(Id, Sel, u32) -> Id =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    f(obj, s, a)
}
/// `objectAtIndex:` — an `NSUInteger` argument.
#[inline]
unsafe fn send_usize(obj: Id, s: Sel, a: usize) -> Id {
    let f: unsafe extern "C" fn(Id, Sel, usize) -> Id =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    f(obj, s, a)
}
/// `count` / `length` — an `NSUInteger` return.
#[inline]
unsafe fn send_ret_usize(obj: Id, s: Sel) -> usize {
    let f: unsafe extern "C" fn(Id, Sel) -> usize =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    f(obj, s)
}
/// `intValue` — an `int` return (the FourCC out of an `NSNumber`).
#[inline]
unsafe fn send_ret_i32(obj: Id, s: Sel) -> i32 {
    let f: unsafe extern "C" fn(Id, Sel) -> i32 =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    f(obj, s)
}
/// `dataWithPropertyList:format:options:error:` — 4 args, an `NSError**` out-parameter.
#[inline]
unsafe fn send_plist(obj: Id, s: Sel, a: Id, fmt: usize, opts: usize, err: *mut Id) -> Id {
    let f: unsafe extern "C" fn(Id, Sel, Id, usize, usize, *mut Id) -> Id =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    f(obj, s, a, fmt, opts, err)
}
/// `initWithAsset:error:` — an `NSError**` out-parameter.
#[inline]
unsafe fn send_id_err(obj: Id, s: Sel, a: Id, err: *mut Id) -> Id {
    let f: unsafe extern "C" fn(Id, Sel, Id, *mut Id) -> Id =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    f(obj, s, a, err)
}
#[inline]
unsafe fn send_ret_cstr(obj: Id, s: Sel) -> *const std::os::raw::c_char {
    let f: unsafe extern "C" fn(Id, Sel) -> *const std::os::raw::c_char =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    f(obj, s)
}
/// `naturalSize` — a `CGSize` (2×f64) return, register-passed on both arches.
#[inline]
unsafe fn send_ret_size(obj: Id, s: Sel) -> CGSize {
    let f: unsafe extern "C" fn(Id, Sel) -> CGSize =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    f(obj, s)
}
/// `preferredTransform` — a 48-byte `CGAffineTransform` return. Indirect on both
/// arches, but the entry point differs: arm64 has no `_stret` (plain `objc_msgSend`
/// handles the x8 indirect return); x86_64 requires `objc_msgSend_stret`.
#[inline]
unsafe fn send_ret_transform(obj: Id, s: Sel) -> CGAffineTransform {
    #[cfg(target_arch = "aarch64")]
    let entry = objc_msgSend as unsafe extern "C" fn();
    #[cfg(target_arch = "x86_64")]
    let entry = objc_msgSend_stret as unsafe extern "C" fn();
    let f: unsafe extern "C" fn(Id, Sel) -> CGAffineTransform = std::mem::transmute(entry);
    f(obj, s)
}
/// A 24-byte `CMTime` return (e.g. `-[AVAsset duration]`). Like the transform above it's
/// an indirect struct return: plain `objc_msgSend` (x8) on arm64, `_stret` on x86_64.
#[inline]
unsafe fn send_ret_cmtime(obj: Id, s: Sel) -> CMTime {
    #[cfg(target_arch = "aarch64")]
    let entry = objc_msgSend as unsafe extern "C" fn();
    #[cfg(target_arch = "x86_64")]
    let entry = objc_msgSend_stret as unsafe extern "C" fn();
    let f: unsafe extern "C" fn(Id, Sel) -> CMTime = std::mem::transmute(entry);
    f(obj, s)
}

// --- RAII guards --------------------------------------------------------------------

/// An autorelease pool scope, popped on drop.
struct Pool(*mut c_void);
impl Pool {
    fn new() -> Self {
        Pool(unsafe { objc_autoreleasePoolPush() })
    }
}
impl Drop for Pool {
    fn drop(&mut self) {
        unsafe { objc_autoreleasePoolPop(self.0) }
    }
}

/// An owned (+1, from `alloc`/`init`) Objective-C object, `release`d on drop —
/// autorelease pools do **not** cover these.
struct Owned(Id);
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { send_void(self.0, sel(c"release")) }
        }
    }
}

/// An owned (+1, copy-rule) CoreFoundation object, `CFRelease`d on drop.
struct OwnedCf(*const c_void);
impl Drop for OwnedCf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) }
        }
    }
}

/// A read-only base-address lock on a `CVPixelBuffer`, unlocked on drop.
struct PixelLock(*const c_void);
impl Drop for PixelLock {
    fn drop(&mut self) {
        unsafe {
            CVPixelBufferUnlockBaseAddress(self.0, LOCK_READ_ONLY);
        }
    }
}

// --- The streaming decoder ------------------------------------------------------------

/// Streaming Live Photo motion decode (task #69): decode the `.mov` at `path` and hand
/// each frame to `emit` as it's ready, per the [`MotionChunk`] state-machine contract.
/// Frames are capped to `max_long_edge` (decode-to-fit) and bounded by
/// [`MAX_MOTION_FRAMES`] / [`MAX_DECODED_BYTES`]. Bails silently (emitting no terminal
/// chunk) once `cancel` is set — the consumer has already dropped the stream.
pub fn decode_live_motion_streaming(
    path: &Path,
    max_long_edge: u32,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(MotionChunk),
) {
    let Ok(cpath) = CString::new(path.as_os_str().as_bytes()) else {
        emit(MotionChunk::Failed(DecodeError::Corrupt(
            "Live Photo path has interior NUL".into(),
        )));
        return;
    };
    let _pool = Pool::new();
    unsafe { stream_inner(&cpath, max_long_edge, cancel, emit) }
}

/// Read-only probe of a video container's first video stream (macOS AVFoundation) — the
/// inspector's video facts: codec, native dimensions, container rotation, frame rate,
/// duration, and whether there's an audio track. The AVFoundation twin of the Windows
/// [`crate::mf_poster::probe_video_stream`], sharing this module's `objc_msgSend` harness.
/// **No frame decode and no RAM read of the file** — it opens an `AVURLAsset` and reads
/// track metadata only (privacy #2: the view path stays read-only). Tens of ms; the
/// caller (the inspector's per-item cache) memoizes the result for the item's lifetime.
pub fn probe_video_stream(path: &Path) -> Result<VideoStreamInfo, DecodeError> {
    let cpath = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| DecodeError::Corrupt("video path has an interior NUL".into()))?;
    let _pool = Pool::new();
    unsafe { probe_inner(&cpath) }
}

/// Build the `AVURLAsset` for a path — the one open both probes share.
unsafe fn make_asset(cpath: &CStr) -> Result<Id, DecodeError> {
    let corrupt = |m: &str| DecodeError::Corrupt(m.to_string());
    let path_ns = send_cstr(
        class(c"NSString"),
        sel(c"stringWithUTF8String:"),
        cpath.as_ptr(),
    );
    if path_ns.is_null() {
        return Err(corrupt("video path is not valid UTF-8"));
    }
    let url = send_id(class(c"NSURL"), sel(c"fileURLWithPath:"), path_ns);
    if url.is_null() {
        return Err(corrupt("could not form a file URL for the video"));
    }
    let asset = send_id2(
        class(c"AVURLAsset"),
        sel(c"URLAssetWithURL:options:"),
        url,
        std::ptr::null_mut(),
    );
    if asset.is_null() {
        return Err(corrupt("AVURLAsset creation failed"));
    }
    Ok(asset)
}

unsafe fn probe_inner(cpath: &CStr) -> Result<VideoStreamInfo, DecodeError> {
    probe_asset(make_asset(cpath)?)
}

unsafe fn probe_asset(asset: Id) -> Result<VideoStreamInfo, DecodeError> {
    let corrupt = |m: &str| DecodeError::Corrupt(m.to_string());
    let vtracks = send_id(asset, sel(c"tracksWithMediaType:"), AVMediaTypeVideo);
    let track = if vtracks.is_null() {
        std::ptr::null_mut()
    } else {
        send(vtracks, sel(c"firstObject"))
    };
    if track.is_null() {
        return Err(corrupt("the container has no video track"));
    }

    // Native (pre-rotation) dimensions + the container rotation quadrant.
    let nat = send_ret_size(track, sel(c"naturalSize"));
    let (width, height) = (nat.width.max(0.0) as u32, nat.height.max(0.0) as u32);
    let t = send_ret_transform(track, sel(c"preferredTransform"));
    let rotation = transform_to_quadrant(t.a, t.b, t.c, t.d)
        .unwrap_or(0)
        .rem_euclid(360) as u32;

    // Frame rate (0.0 = unknown, matching the Windows probe).
    let raw_fps = send_f32(track, sel(c"nominalFrameRate"));
    let fps = if raw_fps > 0.05 { raw_fps as f64 } else { 0.0 };

    // Codec from the first format description's FourCC media subtype.
    let fmts = send(track, sel(c"formatDescriptions"));
    let desc = if fmts.is_null() {
        std::ptr::null_mut()
    } else {
        send(fmts, sel(c"firstObject"))
    };
    let codec = if desc.is_null() {
        "Video"
    } else {
        codec_name_from_fourcc(CMFormatDescriptionGetMediaSubType(desc as *const c_void))
    };

    // Duration (asset-level; NaN/≤0 → unknown).
    let secs = CMTimeGetSeconds(send_ret_cmtime(asset, sel(c"duration")));
    let duration = (secs.is_finite() && secs > 0.0).then(|| Duration::from_secs_f64(secs));

    // Audio-track presence.
    let atracks = send_id(asset, sel(c"tracksWithMediaType:"), AVMediaTypeAudio);
    let has_audio = !atracks.is_null() && !send(atracks, sel(c"firstObject")).is_null();

    Ok(VideoStreamInfo {
        codec,
        width,
        height,
        rotation,
        fps,
        duration,
        has_audio,
        color: ColorTransform::default(),
    })
}

// ---------------------------------------------------------------------------
// Media-track catalog (task #98, phase 2) — the AVFoundation backend.
// ---------------------------------------------------------------------------

/// Probe a video's basic facts **and** its audio/subtitle track catalog from one open.
///
/// Off-thread only. This walks both `AVMediaSelectionGroup`s, which
/// [`probe_video_stream`] deliberately does not — that one feeds the poster path, which
/// runs for every *prefetched* video whether or not the Inspector is ever opened.
///
/// Note the honest scope of this backend, all established by the phase-0 spike
/// (`.taskmaster/docs/98-phase0-spike-findings.md`): AVFoundation exposes *its* model of
/// the file, not the container's stream table. It synthesizes options (forced variants),
/// picks its own `defaultOption`, reports BCP-47 short language tags, and surfaces no
/// per-track title at all (`commonMetadata` is empty even when the container has a
/// `handler_name`). That is why the catalog names its [`MediaBackend`] rather than
/// pretending every backend sees the same thing. Containers AVFoundation can't demux
/// (MKV/WebM) fail at `make_asset`/`probe_asset`, and the caller falls back to the
/// `--ffvideo` FFmpeg probe exactly as playback does.
pub fn probe_video_details(path: &Path, generation: u64) -> Result<VideoDetailsProbe, DecodeError> {
    let cpath = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| DecodeError::Corrupt("video path has an interior NUL".into()))?;
    let _pool = Pool::new();
    unsafe {
        let asset = make_asset(&cpath)?;
        let video = probe_asset(asset)?;
        let tracks = catalog_from_asset(asset, generation);
        Ok(VideoDetailsProbe { video, tracks })
    }
}

/// Guard a selector before sending it.
///
/// Not defensive programming for its own sake: the phase-0 spike proved that sending a
/// selector the class doesn't implement raises an **uncatchable** ObjC exception that
/// aborts the process — `catch_unwind` cannot contain it, so the usual "a hostile file
/// becomes a `DecodeError`" safety net does not apply here. (The specific trap: the
/// property is `playable`, but the getter — and therefore the selector — is `isPlayable`.)
unsafe fn responds(obj: Id, s: Sel) -> bool {
    !obj.is_null() && send_id_ret_bool(obj, sel(c"respondsToSelector:"), s as Id)
}

unsafe fn ns_len(array: Id) -> usize {
    if array.is_null() {
        0
    } else {
        send_ret_usize(array, sel(c"count"))
    }
}

unsafe fn ns_at(array: Id, i: usize) -> Id {
    send_usize(array, sel(c"objectAtIndex:"), i)
}

/// An `NSString` → Rust `String` (`None` for nil/non-UTF-8).
unsafe fn ns_string(s: Id) -> Option<String> {
    if s.is_null() {
        return None;
    }
    let p = send_ret_cstr(s, sel(c"UTF8String"));
    if p.is_null() {
        return None;
    }
    Some(CStr::from_ptr(p).to_string_lossy().into_owned())
}

/// Does `option.mediaCharacteristics` contain `want`? Compares against the framework's
/// own constant rather than a hardcoded literal — the spike found the values are a mix
/// of `public.*` strings and literal `AVMediaCharacteristic*` names, so hardcoding would
/// silently miss half of them.
unsafe fn has_characteristic(chars: Id, want: Id) -> bool {
    if chars.is_null() || want.is_null() {
        return false;
    }
    send_id_ret_bool(chars, sel(c"containsObject:"), want)
}

unsafe fn catalog_from_asset(asset: Id, generation: u64) -> MediaTrackCatalog {
    let mut next_local_id = 0u64;
    let mut locators: Vec<(u64, TrackLocator)> = Vec::new();

    let read_group = |characteristic: Id,
                      group_kind: AvGroup,
                      kind: TrackKind,
                      locators: &mut Vec<(u64, TrackLocator)>,
                      next_local_id: &mut u64|
     -> TrackSet {
        let group = send_id(
            asset,
            sel(c"mediaSelectionGroupForMediaCharacteristic:"),
            characteristic,
        );
        // A nil group means AVFoundation exposes no selection of this kind for this
        // container — NOT that the file has none. `Unavailable` keeps that distinction.
        if group.is_null() {
            return TrackSet::unavailable();
        }
        let options = send(group, sel(c"options"));
        let default_option = send(group, sel(c"defaultOption"));
        let n = ns_len(options);

        let mut tracks = Vec::with_capacity(n);
        for i in 0..n {
            let opt = ns_at(options, i);
            if opt.is_null() {
                continue;
            }
            let local_id = *next_local_id;
            *next_local_id += 1;

            // BCP-47 short ("en"), where the FFmpeg backend reports ISO-639-2 ("eng")
            // for the same file. Both are preserved verbatim; `language_display`
            // resolves either.
            let language = ns_string(send(opt, sel(c"extendedLanguageTag")))
                .and_then(|t| tracks_mod::normalize_lang(&t));

            let chars = send(opt, sel(c"mediaCharacteristics"));
            let flags = TrackFlags {
                // The group's *authored* default. Deliberately not the player's runtime
                // auto-selection, which #99 tracks separately.
                default: !default_option.is_null()
                    && send_id_ret_bool(opt, sel(c"isEqual:"), default_option),
                forced: has_characteristic(chars, AVMediaCharacteristicContainsOnlyForcedSubtitles),
                // AVFoundation has no commentary flag: the spike confirmed a container
                // COMMENT disposition does not survive into any characteristic.
                // `IsAuxiliaryContent` is the nearest real signal; when it's absent we
                // report false rather than guess.
                commentary: has_characteristic(chars, AVMediaCharacteristicIsAuxiliaryContent),
                hearing_impaired: has_characteristic(
                    chars,
                    AVMediaCharacteristicTranscribesSpokenDialogForAccessibility,
                ) || has_characteristic(
                    chars,
                    AVMediaCharacteristicDescribesMusicAndSoundForAccessibility,
                ),
                visual_impaired: has_characteristic(
                    chars,
                    AVMediaCharacteristicDescribesVideoForAccessibility,
                ),
            };

            // The first media subtype's FourCC, normalized into the shared codec
            // vocabulary the pure maps are keyed on.
            let subtypes = send(opt, sel(c"mediaSubTypes"));
            let codec_raw = (ns_len(subtypes) > 0)
                .then(|| send_ret_i32(ns_at(subtypes, 0), sel(c"intValue")) as u32)
                .map(|fourcc| fourcc_to_codec_raw(fourcc, kind))
                .unwrap_or_default();

            let (codec, capability, audio) = match kind {
                TrackKind::Audio => (
                    tracks_mod::audio_codec_display(&codec_raw, None),
                    TrackCapability::Playable,
                    // AVFoundation's option model exposes no channel/rate facts; the
                    // formatter degrades to the codec token rather than inventing them.
                    None,
                ),
                TrackKind::Subtitle => {
                    let playable =
                        responds(opt, sel(c"isPlayable")) && send_ret_bool(opt, sel(c"isPlayable"));
                    let cap = tracks_mod::subtitle_capability(&codec_raw);
                    // An option AVFoundation itself won't play can't be offered, whatever
                    // its codec suggests.
                    let cap = if playable {
                        cap
                    } else {
                        TrackCapability::Unsupported
                    };
                    (tracks_mod::subtitle_codec_display(&codec_raw), cap, None)
                }
            };

            // Identity: the propertyList, serialized to a binary plist. The spike proved
            // it round-trips through `mediaSelectionOptionWithPropertyList:` to an option
            // that `isEqual:` the original — which is what makes #99 able to re-find this
            // exact option later without trusting an ordinal.
            if let Some(property_list) = option_identity(opt) {
                locators.push((
                    local_id,
                    TrackLocator::AvOption {
                        group: group_kind,
                        property_list,
                    },
                ));
            }

            tracks.push(MediaTrack {
                id: TrackId {
                    catalog_generation: generation,
                    local_id,
                },
                kind,
                language,
                // NOT `displayName`: that is a *localized language name* ("English",
                // "French"), not a container title, and treating it as one would put
                // "English" in the title column of every track. AVFoundation surfaces no
                // real title (spike: `commonMetadata` is empty even with a handler_name).
                title: None,
                codec_raw,
                codec,
                capability,
                flags,
                audio,
                external: false, // an option of the container's own asset
            });
        }
        TrackSet::complete(tracks)
    };

    let audio = read_group(
        AVMediaCharacteristicAudible,
        AvGroup::Audible,
        TrackKind::Audio,
        &mut locators,
        &mut next_local_id,
    );
    let subtitles = read_group(
        AVMediaCharacteristicLegible,
        AvGroup::Legible,
        TrackKind::Subtitle,
        &mut locators,
        &mut next_local_id,
    );

    let mut catalog =
        MediaTrackCatalog::new(generation, MediaBackend::AVFoundation, audio, subtitles);
    for (local_id, locator) in locators {
        catalog.set_locator(local_id, locator);
    }
    catalog
}

/// An option's `propertyList`, serialized to a binary plist for [`TrackLocator`].
/// `None` when the option carries no plist or it won't serialize.
unsafe fn option_identity(opt: Id) -> Option<Vec<u8>> {
    if !responds(opt, sel(c"propertyList")) {
        return None;
    }
    let plist = send(opt, sel(c"propertyList"));
    if plist.is_null() {
        return None;
    }
    let mut err: Id = std::ptr::null_mut();
    let data = send_plist(
        class(c"NSPropertyListSerialization"),
        sel(c"dataWithPropertyList:format:options:error:"),
        plist,
        NS_PROPERTY_LIST_BINARY_V1_0,
        0,
        &mut err,
    );
    if data.is_null() {
        return None;
    }
    let len = send_ret_usize(data, sel(c"length"));
    let bytes = send_ret_cstr(data, sel(c"bytes")) as *const u8;
    if bytes.is_null() || len == 0 {
        return None;
    }
    Some(std::slice::from_raw_parts(bytes, len).to_vec())
}

/// A CoreMedia FourCC → the shared `avcodec_get_name`-style codec vocabulary the pure
/// maps in [`crate::tracks`] are keyed on, so one map serves every backend. Unknown
/// FourCCs pass through as their trimmed literal (e.g. `"mp4a"`), which still displays
/// better than a generic label.
fn fourcc_to_codec_raw(fourcc: u32, kind: TrackKind) -> String {
    let raw = String::from_utf8_lossy(&fourcc.to_be_bytes())
        .trim()
        .to_ascii_lowercase();
    let mapped = match (kind, raw.as_str()) {
        (TrackKind::Audio, "aac" | "aacp" | "mp4a" | "paac") => "aac",
        (TrackKind::Audio, "ac-3" | "ac3") => "ac3",
        (TrackKind::Audio, "ec-3" | "ec3") => "eac3",
        (TrackKind::Audio, "lpcm" | "sowt" | "twos" | "in24" | "in32" | "fl32" | "fl64") => {
            "pcm_s16le"
        }
        (TrackKind::Audio, "alac") => "alac",
        (TrackKind::Audio, ".mp3" | "mp3") => "mp3",
        (TrackKind::Audio, "opus") => "opus",
        (TrackKind::Audio, "flac") => "flac",
        (TrackKind::Audio, "dts" | "dtsc" | "dtsh" | "dtsl") => "dts",
        (TrackKind::Audio, "mlpa") => "truehd",
        (TrackKind::Audio, "ima4") => "adpcm_ima_qt",
        (TrackKind::Audio, "qdm2" | "qdmc") => "qdm2",
        (TrackKind::Audio, "ulaw") => "pcm_mulaw",
        (TrackKind::Audio, "alaw") => "pcm_alaw",
        (TrackKind::Subtitle, "tx3g" | "text" | "sbtl") => "mov_text",
        (TrackKind::Subtitle, "wvtt") => "webvtt",
        (TrackKind::Subtitle, "c608") => "eia_608",
        (TrackKind::Subtitle, "c708") => "cea_708",
        (TrackKind::Subtitle, "ssa " | "ass ") => "ass",
        _ => return raw,
    };
    mapped.to_string()
}

/// Map a CoreMedia FourCC video subtype to a display codec name (aligned with the Windows
/// `codec_name` labels). Unknown → "Video".
fn codec_name_from_fourcc(fourcc: u32) -> &'static str {
    match &fourcc.to_be_bytes() {
        b"avc1" | b"avc3" => "H.264",
        b"hvc1" | b"hev1" => "HEVC",
        b"av01" => "AV1",
        b"vp09" => "VP9",
        b"vp08" => "VP8",
        b"mp4v" => "MPEG-4",
        b"mjpg" | b"jpeg" => "Motion JPEG",
        b"apch" | b"apcn" | b"apcs" | b"apco" | b"ap4h" | b"ap4x" => "ProRes",
        _ => "Video",
    }
}

/// Decode the Live Photo motion `.mov` at `path` into a whole [`Animation`], capping each
/// frame's long edge to `max_long_edge` px. A collector over the streaming decoder (which
/// also validates the chunk contract) — kept for the batch callers (`decode_motion_job`,
/// the `live_probe` example).
pub fn decode_live_motion(path: &Path, max_long_edge: u32) -> Result<Animation, DecodeError> {
    let mut collector = MotionCollector::default();
    decode_live_motion_streaming(path, max_long_edge, &AtomicBool::new(false), &mut |chunk| {
        collector.push(chunk)
    });
    collector.finish()
}

macro_rules! fail {
    ($emit:expr, $($arg:tt)*) => {{
        $emit(MotionChunk::Failed(DecodeError::Corrupt(format!($($arg)*))));
        return;
    }};
}

unsafe fn stream_inner(
    cpath: &CStr,
    max_long_edge: u32,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(MotionChunk),
) {
    // Eager-prep streams get superseded fast — don't even open the asset if already stale.
    if cancel.load(Ordering::Relaxed) {
        return;
    }

    // NSURL fileURLWithPath:[NSString stringWithUTF8String:path]  (all autoreleased)
    let path_ns = send_cstr(
        class(c"NSString"),
        sel(c"stringWithUTF8String:"),
        cpath.as_ptr(),
    );
    if path_ns.is_null() {
        fail!(emit, "Live Photo path is not valid UTF-8");
    }
    let url = send_id(class(c"NSURL"), sel(c"fileURLWithPath:"), path_ns);
    let asset = if url.is_null() {
        std::ptr::null_mut()
    } else {
        send_id2(
            class(c"AVURLAsset"),
            sel(c"URLAssetWithURL:options:"),
            url,
            std::ptr::null_mut(),
        )
    };
    if asset.is_null() {
        fail!(emit, "Live Photo motion: AVURLAsset creation failed");
    }
    let tracks = send_id(asset, sel(c"tracksWithMediaType:"), AVMediaTypeVideo);
    let track = if tracks.is_null() {
        std::ptr::null_mut()
    } else {
        send(tracks, sel(c"firstObject"))
    };
    if track.is_null() {
        fail!(emit, "Live Photo motion has no video track");
    }

    // Fallback per-frame delay from the nominal frame rate (real timing comes from each
    // sample's duration below).
    let raw_fps = send_f32(track, sel(c"nominalFrameRate"));
    let fps = if raw_fps > 0.1 { raw_fps as f64 } else { 30.0 };
    let fallback_delay = Duration::from_secs_f64(1.0 / fps);

    // AVAssetReader does not apply preferredTransform — snap it to a rotate_rgba quadrant.
    let t = send_ret_transform(track, sel(c"preferredTransform"));
    let rotation = match transform_to_quadrant(t.a, t.b, t.c, t.d) {
        Some(deg) => deg,
        None => {
            // Accepted degradation: play untransformed (e.g. a mirrored front-camera clip
            // shows un-mirrored) rather than refuse the clip entirely.
            eprintln!(
                "Blaze Viewer: Live Photo preferredTransform [{} {} {} {}] isn't a rotation — playing untransformed",
                t.a, t.b, t.c, t.d
            );
            0
        }
    };

    // Ask VideoToolbox to decode-to-fit (long edge ≤ cap, never upscale). The first
    // decoded buffer is the source of truth — if the request is ignored, a CPU fallback
    // kicks in there.
    let nat = send_ret_size(track, sel(c"naturalSize"));
    let (nw, nh) = (nat.width.max(0.0) as u32, nat.height.max(0.0) as u32);
    let mut want_scale: Option<(u32, u32)> = None;
    if nw > 0 && nh > 0 && max_long_edge > 0 && nw.max(nh) > max_long_edge {
        let s = max_long_edge as f64 / nw.max(nh) as f64;
        want_scale = Some((
            ((nw as f64 * s).round() as u32).max(1),
            ((nh as f64 * s).round() as u32).max(1),
        ));
    }

    // Output settings: exactly BGRA + the optional scale — anything fancier risks an
    // (uncatchable) NSInvalidArgumentException from the track-output initializer.
    let dict = send(class(c"NSMutableDictionary"), sel(c"dictionary"));
    if dict.is_null() {
        fail!(emit, "Live Photo motion: NSMutableDictionary failed");
    }
    let set = sel(c"setObject:forKey:");
    let num_cls = class(c"NSNumber");
    let num_sel = sel(c"numberWithUnsignedInt:");
    send_void_id2(
        dict,
        set,
        send_u32(num_cls, num_sel, PIXEL_BGRA),
        kCVPixelBufferPixelFormatTypeKey as Id,
    );
    if let Some((sw, sh)) = want_scale {
        send_void_id2(
            dict,
            set,
            send_u32(num_cls, num_sel, sw),
            kCVPixelBufferWidthKey as Id,
        );
        send_void_id2(
            dict,
            set,
            send_u32(num_cls, num_sel, sh),
            kCVPixelBufferHeightKey as Id,
        );
    }

    // Reader + track output (+1 each — RAII-released).
    let mut err: Id = std::ptr::null_mut();
    let reader = Owned(send_id_err(
        send(class(c"AVAssetReader"), sel(c"alloc")),
        sel(c"initWithAsset:error:"),
        asset,
        &mut err,
    ));
    if reader.0.is_null() {
        fail!(
            emit,
            "Live Photo motion: AVAssetReader init failed: {}",
            error_desc(err)
        );
    }
    let output = Owned(send_id2(
        send(class(c"AVAssetReaderTrackOutput"), sel(c"alloc")),
        sel(c"initWithTrack:outputSettings:"),
        track,
        dict,
    ));
    if output.0.is_null() {
        fail!(emit, "Live Photo motion: track output init failed");
    }
    // We copy each buffer into a tight RGBA Vec ourselves — no defensive copy needed.
    send_bool(output.0, sel(c"setAlwaysCopiesSampleData:"), false);
    if !send_id_ret_bool(reader.0, sel(c"canAddOutput:"), output.0) {
        fail!(emit, "Live Photo motion: reader rejected the track output");
    }
    send_void_id(reader.0, sel(c"addOutput:"), output.0);

    if cancel.load(Ordering::Relaxed) {
        return;
    }
    if !send_ret_bool(reader.0, sel(c"startReading")) {
        let e = send(reader.0, sel(c"error"));
        fail!(
            emit,
            "Live Photo motion: startReading failed: {}",
            error_desc(e)
        );
    }

    let copy_sel = sel(c"copyNextSampleBuffer");
    let cancel_sel = sel(c"cancelReading");
    let mut count: usize = 0;
    let mut bytes: u64 = 0;
    let mut truncated = false;
    // Set at the first decoded buffer: the header's post-rotation dims (every later
    // frame must match) and the CPU downscale target if VideoToolbox ignored the scale.
    let mut header_dims: Option<(u32, u32)> = None;
    let mut cpu_fit: Option<FitBox> = None;

    loop {
        // Per-iteration pool: a 600-frame AVFoundation loop accumulates autoreleased
        // internals; the outer pool alone would hold them all to the end.
        let _fp = Pool::new();
        if cancel.load(Ordering::Relaxed) {
            send_void(reader.0, cancel_sel);
            return; // silent — no terminal chunk; the consumer dropped the stream
        }
        let sbuf = send(output.0, copy_sel); // +1 (copy rule)
        if sbuf.is_null() {
            break; // EOF or error — classify reader.status below
        }
        let sbuf = OwnedCf(sbuf);
        let img = CMSampleBufferGetImageBuffer(sbuf.0);
        if img.is_null() {
            continue; // not a video sample — skip
        }

        let pw = CVPixelBufferGetWidth(img);
        let ph = CVPixelBufferGetHeight(img);
        let fmt = CVPixelBufferGetPixelFormatType(img);
        if pw == 0 || ph == 0 || fmt != PIXEL_BGRA {
            send_void(reader.0, cancel_sel);
            fail!(
                emit,
                "Live Photo motion: unexpected pixel buffer ({pw}x{ph}, format {fmt:#x})"
            );
        }

        // First decoded buffer: fix the output geometry + color, emit the Header.
        if header_dims.is_none() {
            let (mut ow, mut oh) = (pw as u32, ph as u32);
            if max_long_edge > 0 && ow.max(oh) > max_long_edge {
                // VideoToolbox ignored (or we never sent) the scale request — Lanczos
                // per frame instead. Header dims = the fallback target.
                let fit = FitBox {
                    max_width: max_long_edge,
                    max_height: max_long_edge,
                };
                let s = (fit.max_width as f64 / ow as f64)
                    .min(fit.max_height as f64 / oh as f64)
                    .min(1.0);
                ow = ((ow as f64 * s).round() as u32).max(1);
                oh = ((oh as f64 * s).round() as u32).max(1);
                cpu_fit = Some(fit);
            }
            let (hw, hh) = if rotation == 90 || rotation == 270 {
                (oh, ow)
            } else {
                (ow, oh)
            };
            header_dims = Some((hw, hh));
            emit(MotionChunk::Header(MotionHeader {
                width: hw,
                height: hh,
                color: buffer_color(img),
                codec: "Live Photo",
            }));
        }

        // Copy out (BGRA, row-padded) → tight straight-alpha RGBA.
        let rgba = {
            if CVPixelBufferLockBaseAddress(img, LOCK_READ_ONLY) != 0 {
                send_void(reader.0, cancel_sel);
                fail!(emit, "Live Photo motion: pixel buffer lock failed");
            }
            let _lock = PixelLock(img);
            let base = CVPixelBufferGetBaseAddress(img);
            let stride = CVPixelBufferGetBytesPerRow(img);
            let needed = stride
                .checked_mul(ph - 1)
                .and_then(|v| v.checked_add(pw.checked_mul(4)?));
            let (Some(needed), false) = (needed, base.is_null()) else {
                send_void(reader.0, cancel_sel);
                fail!(emit, "Live Photo motion: bad pixel buffer layout");
            };
            let src = std::slice::from_raw_parts(base as *const u8, needed);
            match bgra_to_rgba_tight(src, pw, ph, stride) {
                Some(v) => v,
                None => {
                    send_void(reader.0, cancel_sel);
                    fail!(emit, "Live Photo motion: bad pixel buffer geometry");
                }
            }
        };

        // Real per-sample duration when the container carries one; nominal fps otherwise.
        let dur = CMSampleBufferGetDuration(sbuf.0);
        let delay = if dur.flags & CM_TIME_VALID != 0 && dur.value > 0 && dur.timescale > 0 {
            Duration::from_secs_f64(dur.value as f64 / dur.timescale as f64)
                .clamp(Duration::from_millis(1), Duration::from_secs(2))
        } else {
            fallback_delay
        };

        // Optional CPU decode-to-fit fallback, then the container rotation.
        let (rgba, fw, fh) = match cpu_fit {
            Some(fit) => match downscale_to_fit(rgba, pw as u32, ph as u32, fit) {
                Ok(v) => v,
                Err(e) => {
                    send_void(reader.0, cancel_sel);
                    fail!(emit, "Live Photo motion: downscale failed: {e}");
                }
            },
            None => (rgba, pw as u32, ph as u32),
        };
        let (rgba, fw, fh) = rotate_rgba(rgba, fw, fh, rotation);
        if header_dims != Some((fw, fh)) {
            send_void(reader.0, cancel_sel);
            fail!(emit, "Live Photo motion: frame size changed mid-stream");
        }

        // Frame + byte budgets, checked *before* emitting — the consumer retains every
        // frame, so the caps bound what it can ever hold.
        let next_bytes = bytes.saturating_add(rgba.len() as u64);
        if count >= MAX_MOTION_FRAMES || next_bytes > MAX_DECODED_BYTES {
            truncated = true;
            send_void(reader.0, cancel_sel);
            break;
        }
        bytes = next_bytes;
        count += 1;
        emit(MotionChunk::Frame(AnimFrame {
            rgba,
            width: fw,
            height: fh,
            delay,
        }));
    }

    // Terminal. Internal truncation also drives the reader to Cancelled, so it must be
    // classified *first* — count ≥ 1 there by construction, and installed playback needs
    // its Done (a missing terminal would park it on the frontier forever).
    if truncated {
        emit(MotionChunk::Done {
            loop_count: 1,
            truncated: true,
        });
        return;
    }
    match send_ret_isize(reader.0, sel(c"status")) {
        READER_COMPLETED => {
            if count == 0 {
                fail!(emit, "Live Photo motion decoded no frames");
            }
            // Plays once and stops (finite loop = 1), like every motion backend.
            emit(MotionChunk::Done {
                loop_count: 1,
                truncated: false,
            });
        }
        READER_FAILED => {
            let e = send(reader.0, sel(c"error"));
            fail!(emit, "Live Photo motion decode failed: {}", error_desc(e));
        }
        READER_CANCELLED => {
            // Not our flag (that path returned above) and not truncation — unexpected.
            fail!(emit, "Live Photo motion: reader cancelled unexpectedly");
        }
        other => fail!(
            emit,
            "Live Photo motion: reader in unexpected state {other}"
        ),
    }
}

/// `NSError` → its localized description (lossy), or a placeholder.
unsafe fn error_desc(err: Id) -> String {
    if !err.is_null() {
        let desc = send(err, sel(c"localizedDescription"));
        if !desc.is_null() {
            let cstr = send_ret_cstr(desc, sel(c"UTF8String"));
            if !cstr.is_null() {
                return CStr::from_ptr(cstr).to_string_lossy().into_owned();
            }
        }
    }
    "(no error description)".into()
}

/// Map the pixel buffer's CoreVideo colorimetry attachments to a [`ColorTransform`] via
/// honest CICP code points. Real Live Photos are BT.709 (transfer 1 → ≈sRGB passthrough)
/// or P3-primaries/709-transfer; missing or unknown attachments fall back to sRGB
/// passthrough (as does anything `from_cicp`/moxcms can't build — including HLG/PQ, which
/// this 8-bit path SDR-clamps by design).
unsafe fn buffer_color(img: *const c_void) -> ColorTransform {
    /// The CICP code point for one colorimetry attachment (`transfer` picks which
    /// CoreVideo mapper to use), or `None` if the attachment is absent/out of range.
    unsafe fn attachment_code(
        img: *const c_void,
        key: *const c_void,
        transfer: bool,
    ) -> Option<u8> {
        let s = OwnedCf(CVBufferCopyAttachment(img, key, std::ptr::null_mut()));
        if s.0.is_null() {
            return None;
        }
        let code = if transfer {
            CVTransferFunctionGetIntegerCodePointForString(s.0)
        } else {
            CVColorPrimariesGetIntegerCodePointForString(s.0)
        };
        u8::try_from(code).ok()
    }
    let (Some(primaries), Some(transfer)) = (
        attachment_code(img, kCVImageBufferColorPrimariesKey, false),
        attachment_code(img, kCVImageBufferTransferFunctionKey, true),
    ) else {
        return ColorTransform::srgb();
    };
    // Matrix 0 (identity/RGB) + full range: the buffer is already RGB — only primaries
    // and transfer matter for the shader transform.
    ColorTransform::from_cicp(primaries, transfer, 0, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracks::TrackCompleteness;
    use std::sync::atomic::AtomicBool;

    fn collect(path: &Path, cancel: &AtomicBool) -> Vec<MotionChunk> {
        let mut chunks = Vec::new();
        decode_live_motion_streaming(path, 1440, cancel, &mut |c| chunks.push(c));
        chunks
    }

    /// A missing file must produce exactly one `Failed` and nothing else.
    #[test]
    fn missing_file_fails_cleanly() {
        let chunks = collect(
            Path::new("/nonexistent/definitely/not/here.mov"),
            &AtomicBool::new(false),
        );
        assert_eq!(chunks.len(), 1);
        assert!(matches!(chunks[0], MotionChunk::Failed(_)));
    }

    /// Garbage bytes must produce exactly one `Failed` — no Header, no Frame, no panic.
    #[test]
    fn garbage_file_fails_cleanly() {
        let dir = std::env::temp_dir().join("pb_livephoto_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("garbage.mov");
        std::fs::write(&path, b"this is definitely not a quicktime movie").unwrap();
        let chunks = collect(&path, &AtomicBool::new(false));
        let _ = std::fs::remove_file(&path);
        assert_eq!(chunks.len(), 1, "expected a single Failed chunk");
        assert!(matches!(chunks[0], MotionChunk::Failed(_)));
    }

    /// A pre-set cancel flag must emit nothing at all (silent non-terminal return).
    #[test]
    fn preset_cancel_emits_nothing() {
        let mov = match std::env::var("PB_LIVE_TEST_MOV") {
            Ok(p) => p,
            Err(_) => {
                eprintln!("PB_LIVE_TEST_MOV not set — skipping");
                return;
            }
        };
        let chunks = collect(Path::new(&mov), &AtomicBool::new(true));
        assert!(chunks.is_empty(), "cancelled stream must emit no chunks");
    }

    /// Cancelling after a few frames stops the stream with no terminal chunk.
    #[test]
    fn midstream_cancel_has_no_terminal() {
        let mov = match std::env::var("PB_LIVE_TEST_MOV") {
            Ok(p) => p,
            Err(_) => {
                eprintln!("PB_LIVE_TEST_MOV not set — skipping");
                return;
            }
        };
        let cancel = AtomicBool::new(false);
        let mut chunks: Vec<MotionChunk> = Vec::new();
        let mut frames = 0usize;
        decode_live_motion_streaming(Path::new(&mov), 1440, &cancel, &mut |c| {
            if matches!(c, MotionChunk::Frame(_)) {
                frames += 1;
                if frames == 3 {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
            chunks.push(c);
        });
        assert!(frames >= 3, "expected at least 3 frames before cancel");
        assert!(
            !matches!(
                chunks.last(),
                Some(MotionChunk::Done { .. }) | Some(MotionChunk::Failed(_))
            ),
            "a cancelled stream must not emit a terminal chunk"
        );
        assert!(frames <= 4, "cancel must stop the stream within a frame");
    }

    /// The full ordering contract on a real clip: Header first, Done last, ≥2 frames,
    /// loop_count 1, nothing after the terminal. Mirrors the ff_live (Linux) test.
    #[test]
    fn streaming_emits_header_then_frames_then_done_in_order() {
        let mov = match std::env::var("PB_LIVE_TEST_MOV") {
            Ok(p) => p,
            Err(_) => {
                eprintln!("PB_LIVE_TEST_MOV not set — skipping");
                return;
            }
        };
        let chunks = collect(Path::new(&mov), &AtomicBool::new(false));
        assert!(chunks.len() >= 4, "expected header + ≥2 frames + done");
        assert!(
            matches!(chunks.first(), Some(MotionChunk::Header(_))),
            "first chunk must be the Header"
        );
        let (mut frames, mut headers) = (0usize, 0usize);
        for (i, c) in chunks.iter().enumerate() {
            match c {
                MotionChunk::Header(_) => headers += 1,
                MotionChunk::Frame(f) => {
                    frames += 1;
                    assert!(!f.rgba.is_empty() && f.width > 0 && f.height > 0);
                }
                MotionChunk::Done {
                    loop_count,
                    truncated,
                } => {
                    assert_eq!(i, chunks.len() - 1, "Done must be the final chunk");
                    assert_eq!(*loop_count, 1, "a Live Photo plays once");
                    assert!(!truncated, "a real Live Photo must not truncate");
                }
                MotionChunk::Failed(e) => panic!("stream failed: {e}"),
            }
        }
        assert_eq!(headers, 1, "exactly one Header");
        assert!(frames >= 2, "expected at least 2 frames, got {frames}");
    }

    /// The batch wrapper agrees with the stream (same clip through both paths).
    #[test]
    fn batch_wrapper_collects_the_stream() {
        let mov = match std::env::var("PB_LIVE_TEST_MOV") {
            Ok(p) => p,
            Err(_) => {
                eprintln!("PB_LIVE_TEST_MOV not set — skipping");
                return;
            }
        };
        let anim = decode_live_motion(Path::new(&mov), 1440).expect("batch decode");
        assert!(anim.frame_count() >= 2);
        assert_eq!(anim.loop_count, 1);
        assert!(anim.width > 0 && anim.height > 0);
        assert!(anim.width.max(anim.height) <= 1440, "decode-to-fit cap");
        let f = &anim.frames[0];
        assert_eq!((f.width, f.height), (anim.width, anim.height));
    }

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/video")
            .join(name)
    }

    /// The FourCC → codec-name map (pure; the values align with the Windows probe labels).
    #[test]
    fn fourcc_maps_to_codec_names() {
        let cc = |s: &[u8; 4]| u32::from_be_bytes(*s);
        assert_eq!(codec_name_from_fourcc(cc(b"avc1")), "H.264");
        assert_eq!(codec_name_from_fourcc(cc(b"hvc1")), "HEVC");
        assert_eq!(codec_name_from_fourcc(cc(b"hev1")), "HEVC");
        assert_eq!(codec_name_from_fourcc(cc(b"av01")), "AV1");
        assert_eq!(codec_name_from_fourcc(cc(b"ap4h")), "ProRes");
        assert_eq!(codec_name_from_fourcc(cc(b"zzzz")), "Video");
    }

    /// The AVFoundation stream probe reports the committed fixture's facts — the macOS
    /// twin of `mf_poster::probe_reports_the_fixtures_stream_facts` (same fixture/values).
    #[test]
    fn probe_reports_the_fixtures_stream_facts() {
        let info = probe_video_stream(&fixture("black_then_color.mp4")).expect("probe");
        assert_eq!(info.codec, "H.264");
        assert_eq!(info.display_dims(), (64, 64));
        assert!(info.fps > 29.0 && info.fps < 31.0, "fps {}", info.fps);
        let dur = info.duration.expect("mp4 reports duration");
        assert!(dur > Duration::from_millis(800) && dur < Duration::from_millis(1500));
        assert!(!info.has_audio);
    }

    // -- media-track catalog (task #98) -------------------------------------

    #[test]
    fn fourcc_maps_into_the_shared_codec_vocabulary() {
        let cc = |b: &[u8; 4]| u32::from_be_bytes(*b);
        assert_eq!(fourcc_to_codec_raw(cc(b"aac "), TrackKind::Audio), "aac");
        assert_eq!(fourcc_to_codec_raw(cc(b"ac-3"), TrackKind::Audio), "ac3");
        assert_eq!(fourcc_to_codec_raw(cc(b"ec-3"), TrackKind::Audio), "eac3");
        assert_eq!(
            fourcc_to_codec_raw(cc(b"lpcm"), TrackKind::Audio),
            "pcm_s16le"
        );
        assert_eq!(fourcc_to_codec_raw(cc(b"alac"), TrackKind::Audio), "alac");
        assert_eq!(
            fourcc_to_codec_raw(cc(b"tx3g"), TrackKind::Subtitle),
            "mov_text"
        );
        assert_eq!(
            fourcc_to_codec_raw(cc(b"wvtt"), TrackKind::Subtitle),
            "webvtt"
        );
        // An unmapped FourCC still says what it is rather than becoming a generic label.
        assert_eq!(fourcc_to_codec_raw(cc(b"zzzz"), TrackKind::Audio), "zzzz");
        // ...and it lands on the shared display map's own fallback.
        assert_eq!(
            crate::tracks::audio_codec_display(
                &fourcc_to_codec_raw(cc(b"aac "), TrackKind::Audio),
                None
            ),
            "AAC"
        );
    }

    /// The catalog over the committed multi-track MP4. Asserts the **AVFoundation
    /// backend's own** view, which the phase-0 spike proved is not the container's
    /// stream table: it synthesizes forced variants (2 authored tx3g tracks → 4 legible
    /// options) and picks its own `defaultOption`. Encoding that here is the point —
    /// if a macOS update changes it, this fails rather than the panel quietly drifting.
    #[test]
    fn catalog_describes_the_multitrack_mp4() {
        let probe = probe_video_details(&fixture("multitrack.mp4"), 9).expect("details probe");
        let cat = &probe.tracks;
        assert_eq!(cat.backend, MediaBackend::AVFoundation);
        assert_eq!(cat.generation, 9);

        // Basic facts still come through the same probe.
        assert_eq!(probe.video.codec, "H.264");
        assert!(probe.video.has_audio);

        // Audible: the 2 real AAC tracks, English authored-default.
        assert_eq!(cat.audio.completeness, TrackCompleteness::Complete);
        assert_eq!(cat.audio.tracks.len(), 2);
        assert!(cat.audio.tracks.iter().all(|t| t.codec == "AAC"));
        assert!(cat.audio.tracks.iter().all(|t| t.kind == TrackKind::Audio));
        // BCP-47 short tags, not the FFmpeg backend's "eng"/"fra".
        assert_eq!(cat.audio.tracks[0].language.as_deref(), Some("en"));
        assert_eq!(cat.audio.tracks[1].language.as_deref(), Some("fr"));
        assert!(cat.audio.tracks[0].flags.default);
        assert!(!cat.audio.tracks[1].flags.default);
        // AVFoundation surfaces no container title, and no commentary flag survives —
        // both honest limits of this backend, not bugs to paper over.
        assert!(cat.audio.tracks.iter().all(|t| t.title.is_none()));
        assert!(cat.audio.tracks.iter().all(|t| t.audio.is_none()));

        // Legible: AVFoundation expands 2 tracks into 4 options (forced variants).
        assert_eq!(cat.subtitles.completeness, TrackCompleteness::Complete);
        assert_eq!(cat.subtitles.tracks.len(), 4);
        assert!(cat.subtitles.tracks.iter().all(|t| t.codec == "Timed Text"));
        assert!(cat
            .subtitles
            .tracks
            .iter()
            .all(|t| t.capability == TrackCapability::SupportedText));
        assert_eq!(
            cat.subtitles
                .tracks
                .iter()
                .filter(|t| t.flags.forced)
                .count(),
            2,
            "the two synthesized forced-only options"
        );
    }

    /// Identity: every track resolves to an `AvOption` locator carrying its group and a
    /// real serialized plist — the thing #99 re-finds an option with.
    #[test]
    fn catalog_locators_carry_a_serialized_property_list() {
        let probe = probe_video_details(&fixture("multitrack.mp4"), 9).expect("details probe");
        let cat = &probe.tracks;
        assert!(cat.tracks().count() > 0);
        for t in cat.tracks() {
            match cat.locator(t.id) {
                Some(TrackLocator::AvOption {
                    group,
                    property_list,
                }) => {
                    let want = match t.kind {
                        TrackKind::Audio => AvGroup::Audible,
                        TrackKind::Subtitle => AvGroup::Legible,
                    };
                    assert_eq!(*group, want);
                    // A real binary plist, not an empty placeholder.
                    assert!(property_list.len() > 8, "plist too short to be real");
                    assert!(property_list.starts_with(b"bplist"), "not a binary plist");
                }
                other => panic!("expected an AvOption locator, got {other:?}"),
            }
        }
        // Local ids are unique across both groups.
        let ids: std::collections::BTreeSet<u64> = cat.tracks().map(|t| t.id.local_id).collect();
        assert_eq!(ids.len(), cat.tracks().count());
    }

    /// A container AVFoundation can't demux errors cleanly, so the caller can fall back
    /// to the FFmpeg probe — it must never be mistaken for "this file has no tracks".
    #[test]
    fn details_probe_errors_on_a_container_avfoundation_cannot_demux() {
        assert!(probe_video_details(&fixture("multitrack.mkv"), 1).is_err());
        assert!(probe_video_details(Path::new("/nonexistent/clip.mp4"), 1).is_err());
    }

    /// A missing file is a clean error, not a panic (hostile-input parity with the decoder).
    #[test]
    fn probe_missing_file_errors_cleanly() {
        assert!(probe_video_stream(Path::new("/nonexistent/clip.mp4")).is_err());
    }
}
