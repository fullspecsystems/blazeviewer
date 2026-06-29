//! CPU HEVC (HEIC) decode via **libheif** + libde265, behind the [`ImageDecoder`]
//! seam and A/B-able against the WIC backend.
//!
//! Why this exists: the Windows WIC HEVC decoder *serializes* — measured ~1.6–2.4×
//! across 8 threads (the GPU's handful of fixed-function decode engines are the
//! shared wall), so the decode pool's 8 workers can't get past ~4–9 HEIC/s. libheif
//! drives libde265, a plain **software** HEVC decoder: no shared GPU session, so N
//! concurrent decodes scale ~linearly across the (otherwise idle) CPU cores. That's
//! what lets us prefetch *full-res* HEICs ahead of the user instead of only
//! sharpening the on-screen one. iPhone/Sony HEICs are 40–48-tile HEVC grids;
//! libheif demuxes + stitches the grid for free (the reason NVDEC is deferred).
//!
//! Scope: **HEIC (HEVC) only.** Our libheif is built decode-only with libde265 (no
//! aom/dav1d), so AVIF stays on WIC; HDR (PQ/HLG) HEICs stay on WIC's fp16 float
//! path; fast scroll **previews** stay on WIC's embedded-thumbnail path. This
//! backend is the *full* SDR HEIC decode — see [`route_full_heic`].
//!
//! The binding is a small hand-rolled `extern "C"` surface (no `libheif-sys`
//! version coupling) linked against the vcpkg static libs by `build.rs`. Every
//! libheif object is owned by a Drop guard so error returns never leak.

use std::ffi::{c_void, CStr};
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::sync::OnceLock;

use crate::isobmff::{
    color_from_colr_box, colr_transfer, has_thumbnail_ref, is_hdr_transfer, isobmff_brand,
};
use crate::{common, ColorTransform, DecodeError, DecodeRequest, DecodedImage, ImageDecoder};

// --- libheif enum constants we use (from heif_image.h / heif_error.h) ---
const HEIF_ERROR_OK: c_int = 0;
const HEIF_COLORSPACE_RGB: c_int = 1;
const HEIF_CHROMA_INTERLEAVED_RGBA: c_int = 11;
const HEIF_CHANNEL_INTERLEAVED: c_int = 10;

// Opaque libheif types (we only ever hold pointers to them).
#[repr(C)]
struct HeifContext {
    _opaque: [u8; 0],
}
#[repr(C)]
struct HeifImageHandle {
    _opaque: [u8; 0],
}
#[repr(C)]
struct HeifImage {
    _opaque: [u8; 0],
}

/// Mirror of C `struct heif_error` (16 bytes; returned by value). The Win64 ABI
/// returns it via hidden pointer, which `extern "C"` + `repr(C)` matches.
#[repr(C)]
struct HeifError {
    code: c_int,
    subcode: c_int,
    message: *const c_char,
}

// Symbols resolve against the static heif.lib/libde265.lib linked by build.rs.
extern "C" {
    fn heif_init(params: *mut c_void) -> HeifError;
    fn heif_context_alloc() -> *mut HeifContext;
    fn heif_context_free(ctx: *mut HeifContext);
    fn heif_context_read_from_memory_without_copy(
        ctx: *mut HeifContext,
        mem: *const c_void,
        size: usize,
        options: *const c_void,
    ) -> HeifError;
    fn heif_context_get_primary_image_handle(
        ctx: *mut HeifContext,
        out_handle: *mut *mut HeifImageHandle,
    ) -> HeifError;
    fn heif_image_handle_release(handle: *const HeifImageHandle);
    fn heif_decode_image(
        handle: *const HeifImageHandle,
        out_img: *mut *mut HeifImage,
        colorspace: c_int,
        chroma: c_int,
        options: *const c_void,
    ) -> HeifError;
    fn heif_image_get_width(img: *const HeifImage, channel: c_int) -> c_int;
    fn heif_image_get_height(img: *const HeifImage, channel: c_int) -> c_int;
    fn heif_image_get_plane_readonly(
        img: *const HeifImage,
        channel: c_int,
        out_stride: *mut c_int,
    ) -> *const u8;
    fn heif_image_release(img: *const HeifImage);
}

// RAII guards so an early error return frees everything (no leaks across the FFI).
struct Ctx(*mut HeifContext);
impl Drop for Ctx {
    fn drop(&mut self) {
        unsafe { heif_context_free(self.0) }
    }
}
struct Handle(*mut HeifImageHandle);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { heif_image_handle_release(self.0) }
    }
}
struct Img(*mut HeifImage);
impl Drop for Img {
    fn drop(&mut self) {
        unsafe { heif_image_release(self.0) }
    }
}

/// libheif's built-in libde265 plugin works without `heif_init`, but calling it
/// once sets up the registry and lets the library free its global pools cleanly.
/// We never `heif_deinit` (process-lifetime; the docs forbid deinit after exit).
fn init_once() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| unsafe {
        let _ = heif_init(ptr::null_mut());
    });
}

unsafe fn check(e: HeifError) -> Result<(), String> {
    if e.code == HEIF_ERROR_OK {
        return Ok(());
    }
    let msg = if e.message.is_null() {
        "libheif error".to_string()
    } else {
        CStr::from_ptr(e.message).to_string_lossy().into_owned()
    };
    Err(format!("{msg} (code {} subcode {})", e.code, e.subcode))
}

/// Decode an HEIC's primary image to tightly-packed interleaved RGBA8 via libheif.
/// libheif applies the container's geometric transforms (rotation/crop/mirror)
/// during decode, so the result is already display-oriented (caller passes
/// orientation 1 to `finalize`).
fn decode_hevc(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), String> {
    init_once();
    unsafe {
        let ctx = Ctx(heif_context_alloc());
        if ctx.0.is_null() {
            return Err("heif_context_alloc returned null".into());
        }
        // NB: we leave libheif's per-context tile threading at its default (on).
        // Measured on this 32-core box with the 8-worker pool: default internal
        // threading wins on *both* single-image latency (112 ms vs 290 ms forcing
        // single-threaded) and 8-way aggregate (43/s vs 23/s) — the internal
        // threads productively fill the cores the 8 workers leave idle. Revisit
        // (`heif_context_set_max_decoding_threads`) only if the pool's worker count
        // is raised toward the core count, where oversubscription would bite.
        // `without_copy` borrows `bytes`; safe because the ctx (and all decoding)
        // is fully done inside this function, before `bytes`/`ctx` go out of scope.
        check(heif_context_read_from_memory_without_copy(
            ctx.0,
            bytes.as_ptr() as *const c_void,
            bytes.len(),
            ptr::null(),
        ))?;

        let mut hptr: *mut HeifImageHandle = ptr::null_mut();
        check(heif_context_get_primary_image_handle(ctx.0, &mut hptr))?;
        if hptr.is_null() {
            return Err("null primary image handle".into());
        }
        let handle = Handle(hptr);

        let mut iptr: *mut HeifImage = ptr::null_mut();
        check(heif_decode_image(
            handle.0,
            &mut iptr,
            HEIF_COLORSPACE_RGB,
            HEIF_CHROMA_INTERLEAVED_RGBA,
            ptr::null(),
        ))?;
        if iptr.is_null() {
            return Err("null decoded image".into());
        }
        let img = Img(iptr);

        let w = heif_image_get_width(img.0, HEIF_CHANNEL_INTERLEAVED);
        let h = heif_image_get_height(img.0, HEIF_CHANNEL_INTERLEAVED);
        if w <= 0 || h <= 0 {
            return Err(format!("bad decoded dimensions {w}x{h}"));
        }
        let (w, h) = (w as usize, h as usize);

        let mut stride: c_int = 0;
        let plane = heif_image_get_plane_readonly(img.0, HEIF_CHANNEL_INTERLEAVED, &mut stride);
        if plane.is_null() {
            return Err("null pixel plane".into());
        }
        let stride = stride as usize;
        let row_bytes = w * 4;
        if stride < row_bytes {
            return Err(format!("stride {stride} < row {row_bytes}"));
        }

        // Copy out row by row, dropping the stride padding into a tight RGBA8 buffer.
        let mut out = vec![0u8; row_bytes * h];
        for y in 0..h {
            let row = std::slice::from_raw_parts(plane.add(y * stride), row_bytes);
            out[y * row_bytes..(y + 1) * row_bytes].copy_from_slice(row);
        }
        Ok((out, w as u32, h as u32))
    }
}

/// CPU HEVC decode backend. `decode` always runs libheif; *routing* (when to pick
/// this over WIC) is [`route_full_heic`], which the dispatcher consults.
#[derive(Debug, Clone, Copy, Default)]
pub struct LibHeifDecoder;

impl ImageDecoder for LibHeifDecoder {
    fn can_decode(&self, bytes: &[u8]) -> bool {
        matches!(isobmff_brand(bytes), Some("HEIC"))
    }

    fn decode(&self, req: &DecodeRequest) -> Result<DecodedImage, DecodeError> {
        let (rgba, w, h) = decode_hevc(req.bytes).map_err(DecodeError::Corrupt)?;
        // libheif self-orients (applies irot/imir), like WIC → pass orientation 1.
        let mut img = common::finalize_oriented(rgba, w, h, 1, "HEIC", req.fit, false)?;
        // Source-native pixels (e.g. Display-P3); carry the colr transform for the
        // in-shader CMS — the exact same path the WIC backend uses.
        img.color = color_from_colr_box(req.bytes).unwrap_or_else(ColorTransform::srgb);
        Ok(img)
    }

    fn name(&self) -> &'static str {
        "libheif"
    }
}

/// Whether the dispatcher should send these bytes to libheif instead of WIC. True
/// when all hold:
/// - the libheif backend is selected (default; `PB_HEIC_BACKEND=wic` forces WIC),
/// - it's an `HEIC` (HEVC) file — AVIF has no AV1 decoder in our libheif build,
/// - it's **not** HDR (PQ/HLG stays on WIC's fp16 scRGB float path),
/// - and it's a **full** decode request, OR a preview request for a HEIC with **no
///   real embedded thumbnail**. WIC's `GetThumbnail` is the fast preview path only
///   when a real `thmb` item exists; otherwise WIC fakes it by decoding the whole
///   grid (~as slow as a full) and we'd then decode the full again — so libheif (one
///   parallel decode) handles those previews too.
pub(crate) fn route_full_heic(bytes: &[u8], allow_preview: bool) -> bool {
    if !backend_is_libheif() {
        return false;
    }
    if !matches!(isobmff_brand(bytes), Some("HEIC")) {
        return false;
    }
    if colr_transfer(bytes).is_some_and(is_hdr_transfer) {
        return false; // HDR → WIC float path
    }
    // Preview of a *thumbnailed* HEIC → WIC's fast GetThumbnail. Everything else
    // (full decodes, and previews of no-thumbnail HEICs) → libheif.
    !(allow_preview && has_thumbnail_ref(bytes))
}

/// Read the `PB_HEIC_BACKEND` A/B switch once. Default (unset) = libheif (the whole
/// point of compiling the feature in); set to `wic` to force the WIC path for an
/// apples-to-apples comparison in one binary.
fn backend_is_libheif() -> bool {
    static SEL: OnceLock<bool> = OnceLock::new();
    *SEL.get_or_init(|| match std::env::var("PB_HEIC_BACKEND") {
        Ok(v) => !v.eq_ignore_ascii_case("wic"),
        Err(_) => true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal `ftyp` header so `isobmff_brand` classifies the major brand.
    fn ftyp(major: &[u8; 4]) -> Vec<u8> {
        let mut v = 16u32.to_be_bytes().to_vec();
        v.extend_from_slice(b"ftyp");
        v.extend_from_slice(major);
        v.extend_from_slice(&[0, 0, 0, 0]); // minor version
        v
    }

    /// `ftyp(heic)` plus a trailing `nclx` `colr` box carrying `transfer` (the
    /// byte `colr_transfer` reads) — enough to exercise the HDR gate.
    fn heic_nclx(transfer: u8) -> Vec<u8> {
        let mut v = ftyp(b"heic");
        v.extend_from_slice(b"colr");
        v.extend_from_slice(b"nclx");
        v.extend_from_slice(&[0, 1]); // primaries
        v.extend_from_slice(&[0, transfer]); // transfer u16 (low byte read)
        v.extend_from_slice(&[0, 0, 0x80]); // matrix + full-range flag
        v
    }

    /// `ftyp(heic)` plus a `thmb` reference 4cc, so `has_thumbnail_ref` sees a real
    /// embedded thumbnail.
    fn heic_with_thumb() -> Vec<u8> {
        let mut v = ftyp(b"heic");
        v.extend_from_slice(b"....iref....thmb...."); // the scanned `thmb` 4cc
        v
    }

    #[test]
    fn routes_full_heic_and_thumbless_previews_to_libheif() {
        // Full SDR HEIC decode → libheif (parallel).
        assert!(route_full_heic(&ftyp(b"heic"), false));
        assert!(route_full_heic(&heic_nclx(13), false)); // sRGB transfer = SDR
                                                         // Preview of a HEIC with NO real thumbnail → libheif too (WIC would fake the
                                                         // thumbnail by decoding the whole grid, then we'd decode the full again).
        assert!(route_full_heic(&ftyp(b"heic"), true));
    }

    #[test]
    fn keeps_thumbnailed_previews_on_wic() {
        // Preview of a HEIC WITH a real thumbnail → WIC's fast GetThumbnail.
        assert!(!route_full_heic(&heic_with_thumb(), true));
        // ...but its full decode still goes to libheif.
        assert!(route_full_heic(&heic_with_thumb(), false));
    }

    #[test]
    fn does_not_route_avif_or_hdr() {
        // AVIF has no AV1 decoder in our libheif build → WIC.
        assert!(!route_full_heic(&ftyp(b"avif"), false));
        assert!(!route_full_heic(&ftyp(b"avif"), true));
        // HDR HEIC (PQ=16, HLG=18) stays on WIC's fp16 float path.
        assert!(!route_full_heic(&heic_nclx(16), false));
        assert!(!route_full_heic(&heic_nclx(18), false));
        // Not an ISOBMFF image at all.
        assert!(!route_full_heic(b"\x89PNG\r\n\x1a\n____________", false));
    }
}
