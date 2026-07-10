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
//! Scope: **SDR HEIC (HEVC), plus AVIF (AV1) when the linked libheif has an AV1
//! decoder.** The Windows vcpkg build is libde265-only (no aom/dav1d), so AVIF there
//! stays on WIC; the Linux system libheif is typically built with dav1d, so it takes
//! AVIF too. HDR (PQ/HLG) stays on WIC's fp16 float path (Windows); fast-scroll
//! **previews** stay on WIC's embedded-thumbnail path. AVIF routing is gated at
//! runtime on [`has_av1_decoder`] — see [`route_full_heic`].
//!
//! The binding is a small hand-rolled `extern "C"` surface (no `libheif-sys`
//! version coupling) linked against the vcpkg static libs by `build.rs`. Every
//! libheif object is owned by a Drop guard so error returns never leak.

use std::ffi::{c_void, CStr};
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::sync::OnceLock;

use crate::isobmff::{color_from_colr_box, colr_transfer, is_hdr_transfer, isobmff_brand};
// The thumbnail carve-out only fires on Windows (it prefers WIC's GetThumbnail).
#[cfg(windows)]
use crate::isobmff::has_thumbnail_ref;
use crate::{common, ColorTransform, DecodeError, DecodeRequest, DecodedImage, ImageDecoder};

// --- libheif enum constants we use (from heif_image.h / heif_error.h) ---
const HEIF_ERROR_OK: c_int = 0;
const HEIF_COLORSPACE_RGB: c_int = 1;
const HEIF_CHROMA_INTERLEAVED_RGBA: c_int = 11;
const HEIF_CHANNEL_INTERLEAVED: c_int = 10;
/// `heif_compression_AV1` (heif.h `enum heif_compression_format`) — probed via
/// `heif_have_decoder_for_format` to learn whether this libheif can decode AVIF.
const HEIF_COMPRESSION_AV1: c_int = 4;

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

/// Mirror of C `struct heif_color_conversion_options` (heif_color.h, version 1 —
/// the header notes it can never grow, so this layout is final).
#[repr(C)]
struct HeifColorConversionOptions {
    version: u8,
    preferred_chroma_downsampling_algorithm: c_int,
    preferred_chroma_upsampling_algorithm: c_int,
    only_use_preferred_chroma_algorithm: u8,
}

/// Mirror of C `struct heif_decoding_options` (heif_decoding.h) through the
/// version-9 fields — just far enough to reach `autocorrect_broken_input`. We
/// never construct one (always `heif_decoding_options_alloc`, so the library owns
/// the real size + version); this mirror only types the field pokes, and the
/// `version >= 9` guard keeps them sound if an older libheif is ever linked.
#[repr(C)]
struct HeifDecodingOptions {
    version: u8,
    ignore_transformations: u8,
    start_progress: *const c_void,
    on_progress: *const c_void,
    end_progress: *const c_void,
    progress_user_data: *mut c_void,
    convert_hdr_to_8bit: u8,
    strict_decoding: u8,
    decoder_id: *const c_char,
    color_conversion_options: HeifColorConversionOptions,
    cancel_decoding: *const c_void,
    color_conversion_options_ext: *mut c_void,
    ignore_sequence_editlist: c_int,
    output_image_nclx_profile: *mut c_void,
    num_library_threads: c_int,
    num_codec_threads: c_int,
    autocorrect_broken_input: u8,
    // version-10+ fields follow in C; never touched here.
}

// Symbols resolve against the static heif.lib/libde265.lib linked by build.rs.
extern "C" {
    fn heif_init(params: *mut c_void) -> HeifError;
    /// Non-zero when a decoder plugin is registered for `format` (e.g. AV1 → dav1d/aom).
    /// Added in libheif 1.15; our build is ≥ 1.21, so it always links.
    fn heif_have_decoder_for_format(format: c_int) -> c_int;
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
    fn heif_decoding_options_alloc() -> *mut HeifDecodingOptions;
    fn heif_decoding_options_free(options: *mut HeifDecodingOptions);
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
struct Opts(*mut HeifDecodingOptions);
impl Drop for Opts {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { heif_decoding_options_free(self.0) }
        }
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

/// Decode the primary image (HEVC for HEIC, AV1 for AVIF — libheif dispatches to the
/// registered plugin) to tightly-packed interleaved RGBA8. libheif applies the
/// container's geometric transforms (rotation/crop/mirror) during decode, so the
/// result is already display-oriented (caller passes orientation 1 to `finalize`).
fn decode_primary(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), String> {
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

        // Sony-quirk workaround (tasks.json #24): some Sony HIF/HEIC files carry an
        // nclx `colr` box whose YCbCr range flag disagrees with the HEVC VUI.
        // Spec-strict libheif follows `colr` and renders ~3.5% darker mid-tones
        // than WIC/Apple (which follow the VUI); `autocorrect_broken_input`
        // (libheif ≥ 1.21, options version 9) opts into the quirk fix. A no-op
        // for conformant files (iPhone HEICs stay pixel-identical to WIC).
        let opts = Opts(heif_decoding_options_alloc());
        if !opts.0.is_null() && (*opts.0).version >= 9 {
            (*opts.0).autocorrect_broken_input = 1;
        }

        let mut iptr: *mut HeifImage = ptr::null_mut();
        check(heif_decode_image(
            handle.0,
            &mut iptr,
            HEIF_COLORSPACE_RGB,
            HEIF_CHROMA_INTERLEAVED_RGBA,
            opts.0 as *const c_void,
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
        match isobmff_brand(bytes) {
            Some("HEIC") => true,
            // AVIF only when this libheif was built with an AV1 decoder (dav1d/aom).
            Some("AVIF") => has_av1_decoder(),
            _ => false,
        }
    }

    fn decode(&self, req: &DecodeRequest) -> Result<DecodedImage, DecodeError> {
        let (rgba, w, h) = decode_primary(req.bytes).map_err(DecodeError::Corrupt)?;
        let codec = match isobmff_brand(req.bytes) {
            Some("AVIF") => "AVIF",
            _ => "HEIC",
        };
        // libheif self-orients (applies irot/imir), like WIC → pass orientation 1.
        let mut img = common::finalize_oriented(rgba, w, h, 1, codec, req.fit, false)?;
        // Source-native pixels (e.g. Display-P3); carry the colr transform for the
        // in-shader CMS — the exact same path the WIC backend uses.
        img.color = color_from_colr_box(req.bytes).unwrap_or_else(ColorTransform::srgb);
        Ok(img)
    }

    fn name(&self) -> &'static str {
        "libheif"
    }
}

/// Whether the dispatcher should send these bytes to libheif. True when all hold:
/// - the libheif backend is selected (default; `PB_HEIC_BACKEND=wic` forces WIC —
///   Windows only, where there's a WIC to fall back to),
/// - it's an `HEIC` (HEVC) file — AVIF has no AV1 decoder in our libheif build,
/// - it's **not** HDR (PQ/HLG stays on WIC's fp16 scRGB float path — Windows only),
/// - and (Windows) it's a **full** decode request, OR a preview request for a HEIC
///   with **no real embedded thumbnail**. WIC's `GetThumbnail` is the fast preview
///   path only when a real `thmb` item exists; otherwise WIC fakes it by decoding
///   the whole grid (~as slow as a full) and we'd then decode the full again — so
///   libheif (one parallel decode) handles those previews too.
///
/// On **Linux** there is no WIC, so the WIC-preference carve-outs (the env switch,
/// the thumbnail-preview exception) don't apply: libheif is the only ISOBMFF decoder,
/// and it takes every SDR HEIC decode — previews included — plus SDR **AVIF** when the
/// system libheif carries an AV1 decoder (the usual dav1d build). HDR (PQ/HLG) still
/// routes away (the 8-bit RGBA path can't hold it) → a graceful `Unsupported`.
pub(crate) fn route_full_heic(bytes: &[u8], allow_preview: bool) -> bool {
    if !backend_is_libheif() {
        return false;
    }
    // HEIC always; AVIF only when the linked libheif actually has an AV1 decoder — so
    // the Windows libde265-only build leaves AVIF to WIC, while a dav1d-built libheif
    // (typical on Linux) decodes it here. A dav1d-less build → routes away → Unsupported.
    let routable = match isobmff_brand(bytes) {
        Some("HEIC") => true,
        Some("AVIF") => has_av1_decoder(),
        _ => false,
    };
    if !routable {
        return false;
    }
    if colr_transfer(bytes).is_some_and(is_hdr_transfer) {
        return false; // HDR → WIC float path (Windows); unsupported on Linux
    }
    // Preview of a *thumbnailed* HEIC → WIC's fast GetThumbnail. No WIC off Windows,
    // so libheif handles previews there too.
    #[cfg(windows)]
    {
        !(allow_preview && has_thumbnail_ref(bytes))
    }
    #[cfg(not(windows))]
    {
        let _ = allow_preview; // no thumbnail carve-out without WIC
        true
    }
}

/// Read the `PB_HEIC_BACKEND` A/B switch once. Default (unset) = libheif (the whole
/// point of compiling the feature in); set to `wic` to force the WIC path for an
/// apples-to-apples comparison in one binary. The switch only makes sense on Windows
/// (the only target with a WIC to fall back to); off Windows libheif is the sole HEIC
/// decoder, so the env var is ignored and this is always `true`.
fn backend_is_libheif() -> bool {
    #[cfg(not(windows))]
    {
        true
    }
    #[cfg(windows)]
    {
        static SEL: OnceLock<bool> = OnceLock::new();
        *SEL.get_or_init(|| match std::env::var("PB_HEIC_BACKEND") {
            Ok(v) => !v.eq_ignore_ascii_case("wic"),
            Err(_) => true,
        })
    }
}

/// Whether the linked libheif has a registered **AV1** decoder plugin — i.e. whether
/// it can decode AVIF. The Linux system libheif is normally built with dav1d (Debian/
/// Ubuntu's `libheif1` depends on `libdav1d`), so this is `true`; the Windows vcpkg
/// static build is libde265-only, so it's `false` and AVIF stays on WIC. Cached — the
/// query walks libheif's plugin registry once.
pub(crate) fn has_av1_decoder() -> bool {
    static AV1: OnceLock<bool> = OnceLock::new();
    *AV1.get_or_init(|| {
        init_once();
        unsafe { heif_have_decoder_for_format(HEIF_COMPRESSION_AV1) != 0 }
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

    #[cfg(windows)]
    #[test]
    fn keeps_thumbnailed_previews_on_wic() {
        // Preview of a HEIC WITH a real thumbnail → WIC's fast GetThumbnail.
        assert!(!route_full_heic(&heic_with_thumb(), true));
        // ...but its full decode still goes to libheif.
        assert!(route_full_heic(&heic_with_thumb(), false));
    }

    #[cfg(not(windows))]
    #[test]
    fn routes_thumbnailed_previews_to_libheif_without_wic() {
        // No WIC to prefer, so even a thumbnailed HEIC's preview goes to libheif
        // (there's no other HEIC decoder on Linux).
        assert!(route_full_heic(&heic_with_thumb(), true));
        assert!(route_full_heic(&heic_with_thumb(), false));
    }

    #[test]
    fn avif_routing_follows_av1_decoder_availability() {
        // SDR AVIF routes to libheif exactly when the linked libheif can decode AV1
        // (dav1d/aom present). The Windows vcpkg build is libde265-only → false → AVIF
        // falls through to WIC; a dav1d-built libheif (typical on Linux) → true.
        let av1 = has_av1_decoder();
        assert_eq!(route_full_heic(&ftyp(b"avif"), false), av1);
        assert_eq!(route_full_heic(&ftyp(b"avif"), true), av1);
    }

    #[test]
    fn does_not_route_hdr_or_non_isobmff() {
        // HDR (PQ=16, HLG=18) stays on WIC's fp16 float path / unsupported off Windows —
        // regardless of codec, and even where AV1 is available.
        assert!(!route_full_heic(&heic_nclx(16), false));
        assert!(!route_full_heic(&heic_nclx(18), false));
        // Not an ISOBMFF image at all.
        assert!(!route_full_heic(b"\x89PNG\r\n\x1a\n____________", false));
    }
}
